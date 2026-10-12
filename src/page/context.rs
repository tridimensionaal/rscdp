use super::Page;
use crate::{
    Error, Result, Session,
    cdp::page::FrameId,
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
    namespace: Option<String>,
    generation: u64,
}

struct World {
    generation: u64,
    candidate: Option<Identity>,
    ready: bool,
    conflicted: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct Identity {
    numeric: i64,
    unique: String,
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
            namespace: None,
            generation: 0,
        })))
    }
}

impl Page {
    pub(super) async fn unique_context_for(&self, frame: &FrameId) -> Result<String> {
        let caller_timeout = self.session.connection().command_timeout();
        let acquire = async {
            let mut state = tokio::select! {
                state = self.worlds.0.clone().lock_owned() => state,
                () = self.session.closed() => return Err(Error::SessionClosed(self.session.id().0.clone())),
            };
            if let Some(unique) = state.prepare(&self.session, &frame.0)? {
                return Ok(unique);
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
                    (Some(numeric), Some(unique)) if !unique.is_empty() => Identity {
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
                    return Err(self.lose_correlation());
                };
                self.frames.retain(|_, world| {
                    world.conflicted
                        || !world
                            .candidate
                            .as_ref()
                            .is_some_and(|identity| identity.unique == unique)
                });
            }
            "Runtime.executionContextsCleared" => self.frames.clear(),
            _ => {}
        }
        Ok(())
    }

    fn stream_error(&mut self, session: &Session, error: Error) -> Error {
        if matches!(error, Error::Lagged(_)) {
            self.frames.clear();
            self.namespace = None;
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

    fn prepare(&mut self, session: &Session, frame: &str) -> Result<Option<String>> {
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
                    .unique
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

    async fn resolve(&mut self, session: &Session, frame: &str, generation: u64) -> Result<String> {
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
                return Ok(identity.unique.clone());
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
