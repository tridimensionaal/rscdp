use super::Page;
use crate::{
    Error, Result,
    cdp::{
        network::LoaderId,
        page::{
            FrameId,
            commands::{GetFrameTree, GetNavigationHistory, NavigateResponse},
            events::{
                FrameDetached, FrameNavigated, FrameStartedNavigating, LifecycleEvent,
                NavigatedWithinDocument,
            },
        },
    },
    connection::{EventOptions, EventStream, RawEvent},
};
use serde_json::json;
use std::time::Duration;
use tokio::time::Instant;

/// The milestone to observe for the document created by a navigation.
///
/// These milestones do not promise application readiness or network idleness.
/// Successful same-document navigation completes immediately for every variant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WaitUntil {
    /// The requested frame is associated with the new loader.
    Commit,
    /// The requested document has fired `DOMContentLoaded`.
    DomContentLoaded,
    /// The requested document has fired `load`.
    #[default]
    Load,
}

/// Configure observation before a click, key press, or history command.
/// Defaults to [`WaitUntil::Load`] and one 30-second budget starting at prepare.
/// Construction has no protocol side effects.
#[must_use = "call prepare().await before performing the action"]
pub struct NavigationObserverBuilder<'a> {
    page: &'a Page,
    wait_until: WaitUntil,
    timeout: Duration,
}

/// One navigation observed after preparation, not proof of what caused it.
#[derive(Debug, Clone)]
pub struct ObservedNavigation {
    /// The root frame captured during preparation.
    pub frame_id: FrameId,
    /// The selected document's loader, or None for same-document navigation.
    pub loader_id: Option<LoaderId>,
    /// URL from the selected commit or same-document event, including its fragment.
    pub url: String,
}

/// An armed observer retaining its Page and exact-session event subscription.
///
/// The first qualifying root commit or same-document event is selected. Another
/// script or a navigation already in flight can win; there is no action ID in CDP
/// events. This does not prove that the action between prepare and wait caused it.
/// No navigation or action is retried.
#[must_use = "perform the action, then call wait().await"]
pub struct PreparedNavigation<'a> {
    _page: &'a Page,
    events: EventStream,
    frame_id: FrameId,
    baseline_loader: LoaderId,
    wait_until: WaitUntil,
    deadline: Instant,
}

const OBSERVATION_METHODS: [&str; 5] = [
    "Page.frameNavigated",
    "Page.navigatedWithinDocument",
    "Page.lifecycleEvent",
    "Page.frameDetached",
    "Page.frameStartedNavigating",
];

enum ObservationEvent {
    Commit(Box<FrameNavigated>),
    SameDocument(NavigatedWithinDocument),
    Lifecycle(LifecycleEvent),
    Detached(FrameDetached),
    Started(FrameStartedNavigating),
}

impl ObservationEvent {
    fn decode(event: RawEvent) -> Result<Self> {
        match event.method.as_str() {
            "Page.frameNavigated" => crate::error::decode(&event.method, event.params)
                .map(|event| Self::Commit(Box::new(event))),
            "Page.navigatedWithinDocument" => {
                crate::error::decode(&event.method, event.params).map(Self::SameDocument)
            }
            "Page.lifecycleEvent" => {
                crate::error::decode(&event.method, event.params).map(Self::Lifecycle)
            }
            "Page.frameDetached" => {
                crate::error::decode(&event.method, event.params).map(Self::Detached)
            }
            "Page.frameStartedNavigating" => {
                crate::error::decode(&event.method, event.params).map(Self::Started)
            }
            _ => Err(Error::UnexpectedResponse {
                method: event.method,
                payload: event.params,
            }),
        }
    }

    fn changes_root(&self, root: &FrameId) -> bool {
        match self {
            Self::Commit(event) => event.frame.id == *root || event.frame.parent_id.is_none(),
            Self::SameDocument(event) => event.frame_id == *root,
            Self::Detached(event) => event.frame_id == *root,
            Self::Started(event) => event.frame_id == *root,
            Self::Lifecycle(_) => false,
        }
    }
}

fn observation_timeout() -> Error {
    Error::Timeout("observing prepared navigation".into())
}

fn check_observation_deadline(deadline: Instant) -> Result<()> {
    if Instant::now() >= deadline {
        return Err(observation_timeout());
    }
    Ok(())
}

impl<'a> NavigationObserverBuilder<'a> {
    pub fn wait_until(mut self, milestone: WaitUntil) -> Self {
        self.wait_until = milestone;
        self
    }

    /// Set one budget for preparation, intervening action time, and waiting.
    /// Zero expires before setup; an unrepresentable deadline is invalid.
    /// The observer cannot preempt an action awaited outside its own future.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Register one ordered stream before lifecycle setup and the frame snapshot.
    ///
    /// Visible root navigation during setup is an error, not a new baseline.
    /// This conservative check is not an atomic browser-side barrier: an earlier
    /// in-flight navigation can still become the first event after preparation.
    /// Shorter session command deadlines still apply. Lifecycle reporting remains
    /// enabled for other listeners after this observer is dropped.
    pub async fn prepare(self) -> Result<PreparedNavigation<'a>> {
        let deadline = Instant::now().checked_add(self.timeout).ok_or_else(|| {
            Error::InvalidArgument("navigation timeout exceeds the monotonic deadline range".into())
        })?;
        check_observation_deadline(deadline)?;
        let prepared = tokio::time::timeout_at(deadline, async {
            let mut events = self
                .page
                .session
                .subscribe_with(EventOptions::default().methods(OBSERVATION_METHODS))?;
            if self.wait_until != WaitUntil::Commit {
                self.page
                    .session
                    .execute_raw("Page.setLifecycleEventsEnabled", json!({"enabled": true}))
                    .await?;
            }
            let root = self
                .page
                .session
                .execute(&GetFrameTree::default())
                .await?
                .frame_tree
                .frame;
            loop {
                check_observation_deadline(deadline)?;
                let Some(event) = events.try_recv()? else {
                    break;
                };
                if ObservationEvent::decode(event)?.changes_root(&root.id) {
                    return Err(Error::Navigation(
                        "root navigation changed during preparation".into(),
                    ));
                }
            }
            Ok(PreparedNavigation {
                _page: self.page,
                events,
                frame_id: root.id,
                baseline_loader: root.loader_id,
                wait_until: self.wait_until,
                deadline,
            })
        })
        .await
        .map_err(|_| observation_timeout())??;
        check_observation_deadline(deadline)?;
        Ok(prepared)
    }
}

impl PreparedNavigation<'_> {
    /// The shared deadline; use with `tokio::time::timeout_at` to bound the action too.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Observe the first root navigation after preparation within the original budget.
    ///
    /// A commit must have a nonempty loader different from the baseline. Load
    /// milestones must follow that commit in stream order and match its frame
    /// and loader. Subframes, old-loader loads, and pre-commit lifecycle events
    /// never satisfy the wait. Same-document events complete at every milestone.
    /// A different root commit or same-document event before the selected
    /// milestone is a supersession error; root detachment/replacement also fails.
    /// Later events do not revoke a milestone already observed in stream order.
    ///
    /// Events buffered during the action are retained, but an expired budget
    /// cannot return success. Lost events, decode errors and session closure
    /// propagate. A download, aborted navigation, or restoration without the
    /// requested lifecycle event may time out. Prefer Commit for BFCache history.
    /// HTTP status is not checked; success does not promise application readiness.
    ///
    /// Cancellation or Drop only stops observation; it does not undo the action,
    /// stop loading, disable events, detach the session, or close the page.
    pub async fn wait(mut self) -> Result<ObservedNavigation> {
        let deadline = self.deadline;
        check_observation_deadline(deadline)?;
        let observed = tokio::time::timeout_at(deadline, async {
            let mut selected: Option<ObservedNavigation> = None;
            loop {
                check_observation_deadline(deadline)?;
                match ObservationEvent::decode(self.events.recv().await?)? {
                    ObservationEvent::Commit(event) => {
                        if event.frame.id != self.frame_id {
                            if event.frame.parent_id.is_none() {
                                return Err(Error::Navigation(
                                    "root frame replaced during navigation observation".into(),
                                ));
                            }
                            continue;
                        }
                        if event.frame.parent_id.is_some() || event.frame.loader_id.0.is_empty() {
                            return Err(Error::Navigation(
                                "root commit has no usable document identity".into(),
                            ));
                        }
                        if let Some(current) = &selected {
                            if current.loader_id.as_ref() != Some(&event.frame.loader_id) {
                                return Err(Error::Navigation(
                                    "observed navigation was superseded".into(),
                                ));
                            }
                            continue;
                        }
                        if event.frame.loader_id == self.baseline_loader {
                            continue;
                        }
                        let navigation = ObservedNavigation {
                            frame_id: event.frame.id,
                            loader_id: Some(event.frame.loader_id),
                            url: format!(
                                "{}{}",
                                event.frame.url,
                                event.frame.url_fragment.unwrap_or_default()
                            ),
                        };
                        if self.wait_until == WaitUntil::Commit {
                            return Ok(navigation);
                        }
                        selected = Some(navigation);
                    }
                    ObservationEvent::SameDocument(event) if event.frame_id == self.frame_id => {
                        if selected.is_some() {
                            return Err(Error::Navigation(
                                "observed navigation was superseded by same-document navigation"
                                    .into(),
                            ));
                        }
                        return Ok(ObservedNavigation {
                            frame_id: event.frame_id,
                            loader_id: None,
                            url: event.url,
                        });
                    }
                    ObservationEvent::Lifecycle(event) => {
                        if let Some(current) = &selected {
                            let name = match self.wait_until {
                                WaitUntil::DomContentLoaded => "DOMContentLoaded",
                                _ => "load",
                            };
                            if event.frame_id == self.frame_id
                                && current.loader_id.as_ref() == Some(&event.loader_id)
                                && event.name == name
                            {
                                return Ok(selected.take().expect("selected navigation"));
                            }
                        }
                    }
                    ObservationEvent::Detached(event) if event.frame_id == self.frame_id => {
                        return Err(Error::Navigation(
                            "root frame detached during navigation observation".into(),
                        ));
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| observation_timeout())??;
        check_observation_deadline(deadline)?;
        Ok(observed)
    }
}

/// A navigation request, defaulting to [`WaitUntil::Load`] and 30 seconds.
///
/// Construction has no protocol side effects. Call [`Self::send`] to navigate.
/// The URL is owned, so a request can be built from a temporary string.
#[must_use = "call send().await to navigate"]
pub struct NavigationBuilder<'a> {
    page: &'a Page,
    url: String,
    wait_until: WaitUntil,
    timeout: Duration,
}

impl NavigationBuilder<'_> {
    /// Choose the milestone for the requested frame and loader.
    pub fn wait_until(mut self, milestone: WaitUntil) -> Self {
        self.wait_until = milestone;
        self
    }

    /// Set the total deadline for setup, the command, and the milestone wait.
    ///
    /// The budget starts when `send()` is polled, not when this builder is made.
    /// Shorter session command deadlines still apply. Zero is an immediate
    /// deadline, not a request to disable timeouts.
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Navigate and return the command response after the selected milestone.
    ///
    /// Subscriptions are registered before enabling lifecycle events or sending
    /// the command, preserving events delivered before its response. Completion
    /// matches both the returned frame and loader; another navigation's events
    /// cannot satisfy this wait. No new loader means a successful same-document
    /// navigation, which completes immediately.
    ///
    /// Downloads and reported navigation failures are errors. HTTP status alone
    /// is not checked. Lost events and session closure propagate as errors.
    /// A superseding navigation may leave this wait pending until its deadline;
    /// subsequent script navigations are not followed and actions are not retried.
    ///
    /// Cancellation or timeout stops waiting, not navigation already received by
    /// Chrome. No stop-loading, detach, or connection-close command is sent.
    /// Lifecycle reporting, when enabled, remains enabled for the shared session
    /// so completing this request does not disrupt other listeners.
    pub async fn send(self) -> Result<NavigateResponse> {
        tokio::time::timeout(self.timeout, async {
            // Both subscriptions precede any command that could emit events.
            let mut commits = self.page.session.listen::<FrameNavigated>();
            let mut lifecycle = self.page.session.listen::<LifecycleEvent>();
            if self.wait_until != WaitUntil::Commit {
                self.page
                    .session
                    .execute_raw("Page.setLifecycleEventsEnabled", json!({"enabled": true}))
                    .await?;
            }
            let response = self.page.start_navigation(&self.url).await?;
            if let Some(loader) = &response.loader_id {
                match self.wait_until {
                    WaitUntil::Commit => loop {
                        let event = commits.recv().await?;
                        if event.frame.id == response.frame_id && event.frame.loader_id == *loader {
                            break;
                        }
                    },
                    WaitUntil::DomContentLoaded | WaitUntil::Load => {
                        let name = match self.wait_until {
                            WaitUntil::DomContentLoaded => "DOMContentLoaded",
                            _ => "load",
                        };
                        loop {
                            let event = lifecycle.recv().await?;
                            if event.frame_id == response.frame_id
                                && event.loader_id == *loader
                                && event.name == name
                            {
                                break;
                            }
                        }
                    }
                }
            }
            Ok(response)
        })
        .await
        .map_err(|_| {
            Error::Timeout(format!(
                "navigating to {} ({:?})",
                self.url, self.wait_until
            ))
        })?
    }
}

impl Page {
    /// Prepare to observe navigation before performing a click, Enter, or history action.
    /// Observe the first qualifying root navigation, not proven action causality.
    ///
    /// ```no_run
    /// # async fn example(page: &rustcdp::Page) -> rustcdp::Result<()> {
    /// use rustcdp::page::WaitUntil;
    /// let navigation = page.expect_navigation()
    ///     .wait_until(WaitUntil::Commit)
    ///     .prepare().await?;
    /// // For click or Enter, perform that action here instead.
    /// if !page.start_go_back().await? {
    ///     drop(navigation); // No preceding history entry: nothing to wait for.
    ///     return Ok(());
    /// }
    /// let observed = navigation.wait().await?;
    /// println!("Observed {}", observed.url);
    /// # Ok(())
    /// # }
    /// ```
    pub fn expect_navigation(&self) -> NavigationObserverBuilder<'_> {
        NavigationObserverBuilder {
            page: self,
            wait_until: WaitUntil::default(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Configure navigation, defaulting to the document's load event and 30 seconds.
    ///
    /// ```no_run
    /// # async fn example(page: &rustcdp::Page) -> rustcdp::Result<()> {
    /// use rustcdp::page::WaitUntil;
    /// use std::time::Duration;
    ///
    /// page.navigate("https://example.com")
    ///     .wait_until(WaitUntil::DomContentLoaded)
    ///     .timeout(Duration::from_secs(10))
    ///     .send()
    ///     .await?;
    /// println!("Loaded {}", page.current_url().await?);
    /// # Ok(())
    /// # }
    /// ```
    pub fn navigate(&self, url: impl Into<String>) -> NavigationBuilder<'_> {
        NavigationBuilder {
            page: self,
            url: url.into(),
            wait_until: WaitUntil::default(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Send navigation and await its protocol response, without a milestone wait.
    ///
    /// This still waits for Chrome's command response; it is not fire-and-forget.
    /// Downloads and reported navigation failures are errors. Prefer
    /// [`Self::navigate`] when document completion is required. Register any
    /// application-owned event listeners before calling this method.
    /// Cancellation cannot undo navigation already received by Chrome.
    pub async fn start_navigation(&self, url: &str) -> Result<NavigateResponse> {
        let response = self
            .session
            .execute_raw(
                "Page.navigate",
                json!({"url": url, "transitionType": "typed"}),
            )
            .await?;
        let response: NavigateResponse = crate::error::decode("Page.navigate", response)?;
        if let Some(error) = &response.error_text {
            return Err(Error::Navigation(error.clone()));
        }
        if response.is_download == Some(true) {
            return Err(Error::Navigation("navigation became a download".into()));
        }
        Ok(response)
    }

    /// Request the preceding history entry, returning false if none exists.
    ///
    /// A true result acknowledges the command, not document completion. History
    /// lookup and navigation are separate commands and are not atomic.
    pub async fn start_go_back(&self) -> Result<bool> {
        self.start_history_navigation(-1).await
    }

    /// Request the following history entry, returning false if none exists.
    ///
    /// A true result acknowledges the command, not document completion. History
    /// lookup and navigation are separate commands and are not atomic.
    pub async fn start_go_forward(&self) -> Result<bool> {
        self.start_history_navigation(1).await
    }

    async fn start_history_navigation(&self, offset: i64) -> Result<bool> {
        let history = self
            .session
            .execute(&GetNavigationHistory::default())
            .await?;
        let Some(index) = history
            .current_index
            .checked_add(offset)
            .and_then(|n| usize::try_from(n).ok())
        else {
            return Ok(false);
        };
        let Some(entry) = history.entries.get(index) else {
            return Ok(false);
        };
        self.session
            .execute_raw("Page.navigateToHistoryEntry", json!({"entryId": entry.id}))
            .await?;
        Ok(true)
    }
}
