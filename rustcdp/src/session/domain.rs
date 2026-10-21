use super::Session;
use crate::{
    Error, Result,
    cdp::{dom, page},
    error::with_cleanup,
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::{Mutex, MutexGuard};

#[derive(Default)]
pub(crate) struct Domains {
    dom: Mutex<DomState>,
    page: Mutex<PageState>,
    runtime: Mutex<usize>,
}

#[derive(Default)]
struct DomState {
    users: usize,
    all_whitespace: bool,
}

#[derive(Default)]
struct PageState {
    users: usize,
    chooser_events: bool,
    explicit_false_users: usize,
}

#[derive(Clone, Copy)]
enum Domain {
    Dom,
    Page { explicit_false: bool },
    Runtime,
}

impl Domain {
    fn name(self) -> &'static str {
        match self {
            Self::Dom => "DOM",
            Self::Page { .. } => "Page",
            Self::Runtime => "Runtime",
        }
    }
}

enum Acquisition {
    Dom(Option<bool>),
    Page(Option<bool>),
    Runtime,
}

const TRANSITION_TIMEOUT: Duration = Duration::from_secs(30);
const DETACH_TIMEOUT: Duration = Duration::from_secs(1);

/// Shared ownership of an enabled DOM, Page, or Runtime domain.
/// Await `close` to confirm the final disable; Drop schedules the same cleanup.
/// Counts and configuration are shared across handles of the same session.
/// Omitted options inherit an active configuration, or select protocol defaults
/// on first enable. Page chooser reporting can be upgraded and remains enabled
/// until final release; DOM whitespace mode cannot change while guards remain.
///
/// Direct enable/disable commands bypass this accounting; do not mix them with
/// guards for the same domain. Acquire a DOM guard before commands that implicitly
/// enable DOM, such as `DOM.getDocument`. Guards do not protect frontend node IDs
/// against navigation or document resets.
///
/// Normal acquisition waits at most 30 seconds for its domain lock. Wire
/// transitions have a separate 30-second limit, independent of command timeouts.
/// A failed enable, upgrade, or final disable triggers a one-second detach
/// attempt; if the session remains open, recovery closes the connection.
/// [`Error::Cleanup`] preserves a transition error and any detach error.
/// No transition is retried. Release lock waits and connection teardown are not
/// included in those bounds.
///
/// Acquisition and release run on the connection runtime independently of caller
/// cancellation. An unclaimed guard is released; a successful Page upgrade may
/// remain active for other owners even when its caller was canceled.
#[must_use = "retain this guard while using the enabled domain; await close() to observe cleanup errors"]
pub struct DomainGuard {
    session: Session,
    domain: Domain,
    active: bool,
    runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for DomainGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainGuard")
            .field("domain", &self.domain.name())
            .field("active", &self.active)
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Enable DOM with no requirement on an already active whitespace mode.
    /// First enable defaults to excluding whitespace-only nodes; later default
    /// acquisitions inherit the active mode. Subscribe before enabling events.
    /// Retain the guard while using the domain. See [`DomainGuard`].
    pub async fn enable_dom(&self) -> Result<DomainGuard> {
        self.enable_dom_with(dom::commands::Enable::default()).await
    }

    /// Acquire DOM with generated enable options.
    /// Omitted `include_whitespace` inherits the active mode, or defaults to
    /// `none` on first enable. Explicit `none`/`all` must match the active mode;
    /// conflicts and unknown wire strings return [`Error::InvalidArgument`]
    /// without sending a command. Known spellings in `Unrecognized` normalize
    /// identically. No live disable/re-enable is performed to change modes.
    /// Release all DOM guards before selecting another mode, then acquire new
    /// frontend node IDs. Subscribe before enabling and retain this guard while
    /// using the domain. Direct enable/disable commands bypass this accounting.
    pub async fn enable_dom_with(&self, options: dom::commands::Enable) -> Result<DomainGuard> {
        let mode = match options
            .include_whitespace
            .as_ref()
            .map(|value| value.as_str())
        {
            None => None,
            Some("none") => Some(false),
            Some("all") => Some(true),
            Some(_) => {
                return Err(Error::InvalidArgument(
                    "DOM guard includeWhitespace must be none or all".into(),
                ));
            }
        };
        self.enable_domain(Acquisition::Dom(mode), serde_json::to_value(options)?)
            .await
    }

    /// Enable Page with no requirement on active chooser reporting.
    /// First enable defaults to false; later default acquisitions inherit the
    /// active setting. This is the neutral guard retained automatically by Page.
    /// Subscribe before enabling events and retain the guard. See [`DomainGuard`].
    pub async fn enable_page(&self) -> Result<DomainGuard> {
        self.enable_page_with(page::commands::Enable::default())
            .await
    }

    /// Acquire Page with generated enable options.
    /// Omission inherits the active setting, or defaults to false on first enable.
    /// Explicit true upgrades reporting unless an explicit-false guard is active.
    /// True remains enabled until final release, even after this guard closes.
    /// Explicit false conflicts with active true and prevents upgrades for its
    /// guard's lifetime. Conflicts return [`Error::InvalidArgument`] locally.
    ///
    /// This works alongside Page's automatically retained neutral guard: listen
    /// first, acquire with `with_enable_file_chooser_opened_event(true)`, then
    /// perform the action. Reporting does not intercept or cancel the native
    /// dialog. False does not suppress events caused by separate interception.
    /// Closing this guard does not disable reporting while another Page-domain
    /// guard remains. Direct enable/disable commands bypass this accounting.
    pub async fn enable_page_with(&self, options: page::commands::Enable) -> Result<DomainGuard> {
        self.enable_domain(
            Acquisition::Page(options.enable_file_chooser_opened_event),
            serde_json::to_value(options)?,
        )
        .await
    }

    /// Enable Runtime events until the last shared guard is released.
    /// Subscribe before acquisition to receive existing-context events emitted
    /// during enablement. Retain the guard while receiving those events. Direct
    /// enable/disable commands bypass accounting; see [`DomainGuard`].
    pub async fn enable_runtime(&self) -> Result<DomainGuard> {
        self.enable_domain(Acquisition::Runtime, json!({})).await
    }

    /// Only for a newly created target whose enclosing owner owns all recovery.
    /// The caller must disable command timeouts, bound setup, and close the target
    /// or dispose its context on failure, including partially completed enables.
    /// Subscribe before calling. No independent task, guard, or recovery is
    /// created until both enables succeed. Lock order is always Page then Runtime.
    pub(crate) async fn enable_page_runtime_for_created_target(&self) -> Result<[DomainGuard; 2]> {
        let mut page = self.domains.page.lock().await;
        let mut runtime = self.domains.runtime.lock().await;
        ensure_open(self)?;
        if page.users == 0 {
            check_response(
                "Page.enable",
                self.execute_raw("Page.enable", json!({})).await?,
            )?;
        }
        if *runtime == 0 {
            check_response(
                "Runtime.enable",
                self.execute_raw("Runtime.enable", json!({})).await?,
            )?;
        }
        if page.users == 0 {
            *page = PageState::default();
        }
        page.users += 1;
        *runtime += 1;
        drop(runtime);
        drop(page);
        Ok([
            Domain::Page {
                explicit_false: false,
            },
            Domain::Runtime,
        ]
        .map(|domain| self.domain_guard(domain)))
    }

    fn domain_guard(&self, domain: Domain) -> DomainGuard {
        DomainGuard {
            session: self.clone().with_command_timeout(None),
            domain,
            active: true,
            runtime: self.connection.runtime(),
        }
    }

    async fn enable_domain(&self, request: Acquisition, params: Value) -> Result<DomainGuard> {
        ensure_open(self)?;
        let session = self.clone().with_command_timeout(None);
        // Keep ownership through transition/recovery after caller cancellation.
        // Tokio drops an unclaimed returned guard, releasing exactly its share.
        self.connection
            .runtime()
            .spawn(async move {
                let domain = acquire(&session, request, params).await?;
                Ok(session.domain_guard(domain))
            })
            .await?
    }
}

async fn acquire(session: &Session, request: Acquisition, params: Value) -> Result<Domain> {
    match request {
        Acquisition::Dom(requested) => {
            let mut state = acquisition_lock(&session.domains.dom).await?;
            ensure_open(session)?;
            if state.users == 0 {
                transition(session, "DOM.enable", params).await?;
                state.all_whitespace = requested.unwrap_or(false);
            } else if requested.is_some_and(|mode| mode != state.all_whitespace) {
                return Err(Error::InvalidArgument(
                    "DOM includeWhitespace conflicts with the active guard configuration".into(),
                ));
            }
            state.users += 1;
            Ok(Domain::Dom)
        }
        Acquisition::Page(requested) => {
            let mut state = acquisition_lock(&session.domains.page).await?;
            ensure_open(session)?;
            if state.users == 0 {
                transition(session, "Page.enable", params).await?;
                state.chooser_events = requested.unwrap_or(false);
                state.explicit_false_users = 0;
            } else {
                match requested {
                    Some(false) if state.chooser_events => {
                        return Err(Error::InvalidArgument(
                            "Page chooser reporting is enabled until final guard release".into(),
                        ));
                    }
                    Some(true) if !state.chooser_events => {
                        if state.explicit_false_users != 0 {
                            return Err(Error::InvalidArgument(
                                "Page chooser reporting conflicts with an explicit-false guard"
                                    .into(),
                            ));
                        }
                        transition(session, "Page.enable", params).await?;
                        state.chooser_events = true;
                    }
                    _ => {}
                }
            }
            let explicit_false = requested == Some(false);
            state.users += 1;
            if explicit_false {
                state.explicit_false_users += 1;
            }
            Ok(Domain::Page { explicit_false })
        }
        Acquisition::Runtime => {
            let mut users = acquisition_lock(&session.domains.runtime).await?;
            ensure_open(session)?;
            if *users == 0 {
                transition(session, "Runtime.enable", params).await?;
            }
            *users += 1;
            Ok(Domain::Runtime)
        }
    }
}

async fn acquisition_lock<T>(mutex: &Mutex<T>) -> Result<MutexGuard<'_, T>> {
    tokio::time::timeout(TRANSITION_TIMEOUT, mutex.lock())
        .await
        .map_err(|_| Error::Timeout("waiting to acquire domain guard".into()))
}

fn ensure_open(session: &Session) -> Result<()> {
    if session.is_closed() {
        Err(Error::SessionClosed(session.id.0.clone()))
    } else {
        Ok(())
    }
}

impl DomainGuard {
    pub async fn close(mut self) -> Result<()> {
        let session = self.session.clone();
        let domain = self.domain;
        let task = self
            .runtime
            .spawn(async move { release(session, domain).await });
        self.active = false;
        task.await?
    }
}

async fn release(session: Session, domain: Domain) -> Result<()> {
    // Release must not abandon ownership while waiting for the domain lock.
    // Keep that lock through final disable and its recovery.
    match domain {
        Domain::Dom => {
            let mut state = session.domains.dom.lock().await;
            if state.users == 0 {
                return Ok(());
            }
            state.users -= 1;
            if state.users == 0 {
                disable(&session, "DOM.disable").await?;
                *state = DomState::default();
            }
        }
        Domain::Page { explicit_false } => {
            let mut state = session.domains.page.lock().await;
            if state.users == 0 {
                return Ok(());
            }
            state.users -= 1;
            if explicit_false {
                state.explicit_false_users -= 1;
            }
            if state.users == 0 {
                disable(&session, "Page.disable").await?;
                *state = PageState::default();
            }
        }
        Domain::Runtime => {
            let mut users = session.domains.runtime.lock().await;
            if *users == 0 {
                return Ok(());
            }
            *users -= 1;
            if *users == 0 {
                disable(&session, "Runtime.disable").await?;
            }
        }
    }
    Ok(())
}

async fn disable(session: &Session, method: &str) -> Result<()> {
    if session.is_closed() {
        return Ok(());
    }
    transition(session, method, json!({})).await
}

async fn transition(session: &Session, method: &str, params: Value) -> Result<()> {
    let result = tokio::time::timeout(TRANSITION_TIMEOUT, session.execute_raw(method, params))
        .await
        .unwrap_or_else(|_| Err(Error::Timeout(method.into())))
        .and_then(|payload| check_response(method, payload));
    match result {
        Ok(()) => Ok(()),
        // A dispatched failure can leave no owner or an unknown upgraded setting.
        // Recover before allowing any acquisition to reuse this session state.
        Err(error) => with_cleanup(Err(error), recover(session).await),
    }
}

fn check_response(method: &str, payload: Value) -> Result<()> {
    if payload.is_object() {
        Ok(())
    } else {
        Err(Error::UnexpectedResponse {
            method: method.into(),
            payload,
        })
    }
}

async fn recover(session: &Session) -> Result<()> {
    if session.is_closed() {
        return Ok(());
    }
    let detached = tokio::time::timeout(DETACH_TIMEOUT, session.detach())
        .await
        .unwrap_or_else(|_| {
            Err(Error::Timeout(
                "detaching session after domain transition failure".into(),
            ))
        });
    if !session.is_closed() {
        session.connection().close().await;
    }
    detached
}

impl Drop for DomainGuard {
    fn drop(&mut self) {
        if self.active {
            let session = self.session.clone();
            let domain = self.domain;
            self.runtime.spawn(async move {
                let _ = release(session, domain).await;
            });
        }
    }
}
