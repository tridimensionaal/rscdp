use super::Page;
use crate::{
    Error, Result, Session,
    cdp::page::{FrameId, commands::GetFrameTree},
    connection::{EventStream, RawEvent},
};
use serde_json::json;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

const METHODS: [&str; 3] = [
    "Runtime.executionContextCreated",
    "Runtime.executionContextDestroyed",
    "Runtime.executionContextsCleared",
];
const SETUP_TIMEOUT: Duration = Duration::from_secs(30);
static NEXT_WORLD: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub(super) struct Worlds(Arc<Mutex<State>>);

struct State {
    events: EventStream,
    frames: HashMap<String, World>,
    main: MainContexts,
    namespace: Option<String>,
    generation: u64,
}

struct World {
    generation: u64,
    candidate: Option<ContextIdentity>,
    ready: bool,
    conflicted: bool,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ContextIdentity {
    pub(super) numeric: i64,
    pub(super) unique: String,
}

// A None entry is an ambiguous frame, retained until contexts are cleared.
// After event loss, only frames with fresh creation events can be used.
// BFCache may announce a previously seen identity again after a clear. Accept
// that fresh event, but never choose between overlapping default identities.
#[derive(Default)]
struct MainContexts {
    frames: HashMap<String, Option<ContextIdentity>>,
    lost: bool,
}

fn invalid_main_context() -> Error {
    Error::InvalidArgument(
        "main execution context correlation unavailable or ambiguous; navigate or reattach the page"
            .into(),
    )
}

impl MainContexts {
    fn lose_correlation(&mut self) {
        self.frames.clear();
        self.lost = true;
    }

    fn created(&mut self, context: &serde_json::Value) {
        if context["auxData"]["isDefault"].as_bool() != Some(true) {
            return;
        }
        let Some(frame) = context["auxData"]["frameId"]
            .as_str()
            .filter(|frame| !frame.is_empty())
        else {
            self.lose_correlation();
            return;
        };
        let identity = match (context["id"].as_i64(), context["uniqueId"].as_str()) {
            (Some(numeric), Some(unique)) if !unique.is_empty() => ContextIdentity {
                numeric,
                unique: unique.into(),
            },
            _ => {
                self.frames.insert(frame.into(), None);
                return;
            }
        };
        self.frames
            .entry(frame.into())
            .and_modify(|old| {
                if old.as_ref() != Some(&identity) {
                    *old = None;
                }
            })
            .or_insert(Some(identity));
    }

    fn destroyed(&mut self, unique: &str) {
        self.frames.retain(|_, identity| {
            !identity
                .as_ref()
                .is_some_and(|identity| identity.unique == unique)
        });
    }

    fn get(&self, frame: &str) -> Result<Option<ContextIdentity>> {
        match self.frames.get(frame) {
            Some(Some(identity)) => Ok(Some(identity.clone())),
            Some(None) => Err(invalid_main_context()),
            None if self.lost => Err(invalid_main_context()),
            None => Ok(None),
        }
    }
}

fn subscribe(session: &Session) -> EventStream {
    session
        .connection()
        .subscribe_filtered(&METHODS, Some(session.id().0.clone()), 256)
}

fn invalid_context() -> Error {
    Error::InvalidArgument("invalid isolated execution context correlation".into())
}

impl Worlds {
    // Must run before Runtime.enable, including context-owned Page setup.
    pub(super) fn new(session: &Session) -> Self {
        Self(Arc::new(Mutex::new(State {
            events: subscribe(session),
            frames: HashMap::new(),
            main: MainContexts::default(),
            namespace: None,
            generation: 0,
        })))
    }
}

impl Page {
    /// Resolve only an event-correlated default context. Callers must evaluate
    /// with its unique ID, never its numeric ID or Runtime's implicit default.
    pub(super) async fn main_context(&self) -> Result<ContextIdentity> {
        let timeout = self
            .session
            .connection()
            .command_timeout()
            .unwrap_or(SETUP_TIMEOUT)
            .min(SETUP_TIMEOUT);
        tokio::time::timeout(timeout, async {
            let frame = self
                .session
                .execute(&GetFrameTree::default())
                .await?
                .frame_tree
                .frame
                .id;
            self.main_context_for(&frame).await
        })
        .await
        .map_err(|_| {
            Error::Timeout("acquiring main execution context; navigate or reattach the page".into())
        })?
    }

    pub(super) async fn main_context_for(&self, frame: &FrameId) -> Result<ContextIdentity> {
        let timeout = self
            .session
            .connection()
            .command_timeout()
            .unwrap_or(SETUP_TIMEOUT)
            .min(SETUP_TIMEOUT);
        let acquire = async {
            let mut state = self.worlds.0.lock().await;
            loop {
                // Drain before returning a cached identity: a queued destroy,
                // clear, or second default context must invalidate it first.
                if let Err(error) = state.drain(&self.session) {
                    return Err(if matches!(error, Error::Lagged(_)) {
                        invalid_main_context()
                    } else {
                        error
                    });
                }
                if let Some(identity) = state.main.get(&frame.0)? {
                    return Ok(identity);
                }
                let event = state.events.recv().await.map_err(|error| {
                    let error = state.stream_error(&self.session, error);
                    if matches!(error, Error::Lagged(_)) {
                        invalid_main_context()
                    } else {
                        error
                    }
                })?;
                state.handle(event)?;
            }
        };
        // No world-creation command is needed. Canceling this read-only wait is
        // safe; the borrowed Page retains its lease and recv is cancellation-safe.
        // The deadline includes the mutex queue, even with command timeouts off.
        tokio::select! {
            biased;
            () = self.session.closed() => Err(Error::SessionClosed(self.session.id().0.clone())),
            result = tokio::time::timeout(timeout, acquire) => {
                result.map_err(|_| Error::Timeout(
                    "waiting for main execution context; navigate or reattach the page".into(),
                ))?
            }
        }
    }

    pub(super) async fn isolated_context(&self) -> Result<ContextIdentity> {
        let frame = self
            .session
            .execute(&GetFrameTree::default())
            .await?
            .frame_tree
            .frame
            .id;
        self.isolated_context_for(&frame).await
    }

    pub(super) async fn isolated_context_for(&self, frame: &FrameId) -> Result<ContextIdentity> {
        let caller_timeout = self.session.connection().command_timeout();
        let acquire = async {
            let mut state = tokio::select! {
                state = self.worlds.0.clone().lock_owned() => state,
                () = self.session.closed() => return Err(Error::SessionClosed(self.session.id().0.clone())),
            };
            if let Some(identity) = state.prepare(&self.session, &frame.0)? {
                return Ok(identity);
            }
            let generation = state.frames[&frame.0].generation;
            let page = self.clone();
            let frame = frame.0.clone();
            let task = self.session.connection().runtime().spawn(async move {
                // Only a miss transfers the lock and Page to an owner. Keep the
                // entire Page (including its context lease) until setup ends.
                let session = page.session.clone().with_command_timeout(None);
                let result = tokio::select! {
                    result = tokio::time::timeout(SETUP_TIMEOUT, state.resolve(&session, &frame, generation)) => {
                        result.map_err(|_| Error::Timeout("acquiring isolated execution context".into()))?
                    }
                    () = session.closed() => Err(Error::SessionClosed(session.id().0.clone())),
                };
                drop(page);
                result
            });
            task.await?
        };
        // One caller deadline covers both the mutex queue and the owner wait.
        // Dropping the JoinHandle on timeout leaves an actual miss owned.
        if let Some(timeout) = caller_timeout {
            tokio::time::timeout(timeout, acquire)
                .await
                .map_err(|_| Error::Timeout("waiting for isolated execution context".into()))?
        } else {
            acquire.await
        }
    }
}

impl State {
    fn ensure_namespace(&mut self, session: &Session) -> Result<()> {
        if self.namespace.is_none() {
            let nonce = NEXT_WORLD
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| Error::InvalidArgument("isolated world names exhausted".into()))?;
            self.namespace = Some(format!("rustcdp-private-{}-{nonce}-", session.id().0));
        }
        Ok(())
    }

    fn next_generation(&mut self) -> Result<u64> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| Error::InvalidArgument("isolated world generations exhausted".into()))?;
        Ok(self.generation)
    }

    fn lose_correlation(&mut self) -> Error {
        self.frames.clear();
        self.namespace = None;
        invalid_context()
    }

    fn handle(&mut self, event: RawEvent) -> Result<()> {
        match event.method.as_str() {
            "Runtime.executionContextCreated" => {
                let context = &event.params["context"];
                self.main.created(context);
                let Some(frame) = self
                    .namespace
                    .as_ref()
                    .and_then(|prefix| context["name"].as_str()?.strip_prefix(prefix))
                else {
                    return Ok(());
                };
                if frame.is_empty() || context["auxData"]["frameId"].as_str() != Some(frame) {
                    return Err(self.lose_correlation());
                }
                let frame = frame.to_owned();
                let identity = match (context["id"].as_i64(), context["uniqueId"].as_str()) {
                    (Some(numeric), Some(unique)) if !unique.is_empty() => ContextIdentity {
                        numeric,
                        unique: unique.into(),
                    },
                    _ => return Err(self.lose_correlation()),
                };
                if let Some(world) = self.frames.get_mut(&frame) {
                    if world.candidate.as_ref().is_some_and(|old| old != &identity) {
                        // Two live identities for the same name are ambiguous even
                        // if a later command returns the same numeric ID. Preserve
                        // the conflict until clear or exceptional namespace reset.
                        world.conflicted = true;
                        world.ready = false;
                    } else if !world.conflicted {
                        world.candidate = Some(identity);
                    }
                } else {
                    let generation = self.next_generation()?;
                    self.frames.insert(
                        frame,
                        World {
                            generation,
                            candidate: Some(identity),
                            ready: false,
                            conflicted: false,
                        },
                    );
                }
            }
            "Runtime.executionContextDestroyed" => {
                let Some(unique) = event.params["executionContextUniqueId"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                else {
                    self.main.lose_correlation();
                    return Err(self.lose_correlation());
                };
                self.main.destroyed(unique);
                self.frames.retain(|_, world| {
                    world.conflicted
                        || !world
                            .candidate
                            .as_ref()
                            .is_some_and(|identity| identity.unique == unique)
                });
            }
            "Runtime.executionContextsCleared" => {
                self.frames.clear();
                self.main = MainContexts::default();
            }
            _ => {}
        }
        Ok(())
    }

    fn stream_error(&mut self, session: &Session, error: Error) -> Error {
        if matches!(error, Error::Lagged(_)) {
            self.main.lose_correlation();
            self.frames.clear();
            self.namespace = None;
            // Discard retained events too: a creation in the old queue may
            // precede a lost destroy. Never replay Runtime.enable to recover.
            self.events = subscribe(session);
        }
        error
    }

    fn drain(&mut self, session: &Session) -> Result<()> {
        loop {
            match self.events.try_recv() {
                Ok(Some(event)) => self.handle(event)?,
                Ok(None) => return Ok(()),
                Err(error) => return Err(self.stream_error(session, error)),
            }
        }
    }

    fn prepare(&mut self, session: &Session, frame: &str) -> Result<Option<ContextIdentity>> {
        self.ensure_namespace(session)?;
        self.drain(session)?;
        if !self.frames.contains_key(frame) {
            let generation = self.next_generation()?;
            self.frames.insert(
                frame.into(),
                World {
                    generation,
                    candidate: None,
                    ready: false,
                    conflicted: false,
                },
            );
        }
        let world = &self.frames[frame];
        if world.conflicted {
            return Err(invalid_context());
        }
        if world.ready {
            return Ok(Some(
                world
                    .candidate
                    .as_ref()
                    .expect("ready world identity")
                    .clone(),
            ));
        }
        Ok(None)
    }

    fn check_generation(&self, frame: &str, generation: u64) -> Result<()> {
        let world = self
            .frames
            .get(frame)
            .ok_or(Error::ExecutionContextChanged)?;
        if world.generation != generation {
            return Err(Error::ExecutionContextChanged);
        }
        if world.conflicted {
            return Err(invalid_context());
        }
        Ok(())
    }

    async fn resolve(
        &mut self,
        session: &Session,
        frame: &str,
        generation: u64,
    ) -> Result<ContextIdentity> {
        self.drain(session)?;
        self.check_generation(frame, generation)?;
        // Names survive normal navigation: Chrome retains named worlds per frame.
        // Timeout also retains the candidate; the next call still requires a fresh
        // numeric reply correlated with that candidate before marking it ready.
        let name = format!(
            "{}{frame}",
            self.namespace.as_ref().expect("prepared namespace")
        );
        let command = session.execute_raw(
            "Page.createIsolatedWorld",
            json!({
                "frameId": frame, "worldName": name,
            }),
        );
        tokio::pin!(command);
        let numeric = loop {
            tokio::select! {
                response = &mut command => {
                    let response = response?;
                    let Some(numeric) = response["executionContextId"].as_i64() else {
                        return Err(self.lose_correlation());
                    };
                    break numeric;
                }
                event = self.events.recv() => {
                    let event = event.map_err(|error| self.stream_error(session, error))?;
                    self.handle(event)?;
                    self.check_generation(frame, generation)?;
                }
            }
        };
        loop {
            self.drain(session)?;
            self.check_generation(frame, generation)?;
            let world = self.frames.get_mut(frame).expect("checked generation");
            if let Some(identity) = &world.candidate {
                if identity.numeric != numeric {
                    world.conflicted = true;
                    return Err(invalid_context());
                }
                world.ready = true;
                return Ok(identity.clone());
            }
            let event = self
                .events
                .recv()
                .await
                .map_err(|error| self.stream_error(session, error))?;
            self.handle(event)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn created(frame: &str, numeric: i64, unique: &str) -> serde_json::Value {
        json!({"id": numeric, "uniqueId": unique,
            "auxData": {"frameId": frame, "isDefault": true}})
    }

    #[test]
    fn main_context_requires_default_frame_and_unique_identity() {
        let mut main = MainContexts::default();
        let mut isolated = created("frame", 1, "isolated");
        isolated["auxData"]["isDefault"] = json!(false);
        main.created(&isolated);
        assert!(main.get("frame").unwrap().is_none());
        main.created(&created("frame", 1, "main"));
        assert_eq!(main.get("frame").unwrap().unwrap().unique, "main");
        main.created(&created("other", 2, ""));
        assert!(main.get("other").is_err());
        assert!(main.get("frame").unwrap().is_some());
    }

    #[test]
    fn main_context_destroy_uses_unique_id_even_when_numeric_id_is_reused() {
        let mut main = MainContexts::default();
        main.created(&created("frame", 1, "old"));
        main.destroyed("old");
        assert!(main.get("frame").unwrap().is_none());
        main.created(&created("frame", 1, "new"));
        main.destroyed("old");
        assert_eq!(main.get("frame").unwrap().unwrap().unique, "new");
    }

    #[test]
    fn overlapping_main_contexts_remain_ambiguous_until_clear() {
        let mut main = MainContexts::default();
        main.created(&created("frame", 1, "a"));
        main.created(&created("frame", 1, "a"));
        assert!(main.get("frame").unwrap().is_some());
        main.created(&created("frame", 2, "b"));
        assert!(main.get("frame").is_err());
        main.destroyed("a");
        main.created(&created("frame", 2, "b"));
        assert!(main.get("frame").is_err());
        main = MainContexts::default();
        assert!(main.get("frame").unwrap().is_none());
        main.created(&created("frame", 1, "a"));
        assert_eq!(main.get("frame").unwrap().unwrap().unique, "a");
    }

    #[test]
    fn lost_main_correlation_requires_fresh_events_per_frame() {
        let mut main = MainContexts::default();
        main.created(&created("frame", 1, "old"));
        main.lose_correlation();
        assert!(main.get("frame").is_err());
        main.created(&created("other", 2, "fresh"));
        assert!(main.get("frame").is_err());
        assert_eq!(main.get("other").unwrap().unwrap().unique, "fresh");
        main.created(&created("frame", 3, "new"));
        assert_eq!(main.get("frame").unwrap().unwrap().unique, "new");
    }

    #[test]
    fn uncorrelated_default_event_invalidates_all_main_frames() {
        let mut main = MainContexts::default();
        main.created(&created("frame", 1, "old"));
        let mut invalid = created("other", 2, "new");
        invalid["auxData"]["frameId"] = json!(null);
        main.created(&invalid);
        assert!(main.get("frame").is_err());
    }
}
