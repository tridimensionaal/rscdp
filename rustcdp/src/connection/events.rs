use super::{Connection, EventStream};
use crate::{Error, Result};

/// Which event envelopes a connection subscription observes.
/// Use [`crate::Session::subscribe_with`] for one attached session with ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventScope {
    /// Root and all sessions, including session IDs not adopted by this client.
    All,
    /// Only envelopes without a session ID, regardless of IDs inside parameters.
    Root,
}

/// Filters and buffer size for a raw event subscription.
///
/// Registration is synchronous; these options never enable a protocol domain.
/// Setters replace earlier values. Invalid options fail even on a closed handle.
#[derive(Debug, Clone, Default)]
#[must_use]
pub struct EventOptions {
    /// Exact, case-sensitive wire methods. Empty selects all methods.
    /// Duplicate names are delivered once; an empty name is invalid.
    pub methods: Vec<String>,
    /// Matching-event capacity, or None to inherit the connection default.
    /// Must be in 1..=usize::MAX / 2; Tokio rounds up to a power of two.
    /// Counts events, not bytes. Valid large capacities can still exhaust memory.
    pub capacity: Option<usize>,
}

impl EventOptions {
    pub fn methods<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.methods = methods.into_iter().map(Into::into).collect();
        self
    }

    pub fn capacity(mut self, capacity: usize) -> Self {
        self.capacity = Some(capacity);
        self
    }

    fn validated_capacity(&self, default: usize) -> Result<usize> {
        if self.methods.iter().any(String::is_empty) {
            return Err(Error::InvalidArgument(
                "event method must not be empty".into(),
            ));
        }
        let capacity = self.capacity.unwrap_or(default);
        validate_capacity(capacity)?;
        Ok(capacity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum ScopeKey {
    All,
    Exact(Option<String>),
}

fn validate_capacity(capacity: usize) -> Result<()> {
    if capacity == 0 || capacity > usize::MAX / 2 {
        return Err(Error::InvalidArgument(format!(
            "event capacity must be in 1..={}",
            usize::MAX / 2
        )));
    }
    Ok(())
}

impl Connection {
    /// Register a raw subscription with explicit scope and pre-buffer filtering.
    ///
    /// Matching methods share one buffer in receive order. Unrelated events do
    /// not consume its capacity. Neither scope retains session attachments.
    /// Empty methods select all; duplicates are deduplicated. No replay or domain
    /// enablement occurs. Overflow reports [`Error::Lagged`].
    pub fn subscribe_with(&self, scope: EventScope, options: EventOptions) -> Result<EventStream> {
        let scope = match scope {
            EventScope::All => ScopeKey::All,
            EventScope::Root => ScopeKey::Exact(None),
        };
        self.subscribe_options(scope, options)
    }

    pub(crate) fn subscribe_session_with(
        &self,
        id: &str,
        options: EventOptions,
    ) -> Result<EventStream> {
        self.subscribe_options(ScopeKey::Exact(Some(id.into())), options)
    }

    pub(crate) fn subscribe_typed_with_capacity(
        &self,
        method: &str,
        session: Option<String>,
        capacity: usize,
    ) -> Result<EventStream> {
        validate_capacity(capacity)?;
        Ok(self.subscribe_filtered(&[method], session, capacity))
    }

    fn subscribe_options(&self, scope: ScopeKey, options: EventOptions) -> Result<EventStream> {
        let capacity = options.validated_capacity(self.0.event_capacity)?;
        if scope == ScopeKey::All && options.methods.is_empty() && options.capacity.is_none() {
            // Preserve the existing shared global queue for default observers.
            return Ok(self.subscribe());
        }
        let methods = (!options.methods.is_empty()).then(|| {
            options
                .methods
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        });
        Ok(self.subscribe_inner(methods.as_deref(), scope, capacity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_event_options_fail_before_channel_creation() {
        for options in [
            EventOptions::default().capacity(0),
            EventOptions::default().capacity(usize::MAX),
            EventOptions::default().methods(["Example.event", ""]),
        ] {
            assert!(matches!(
                options.validated_capacity(256),
                Err(Error::InvalidArgument(_))
            ));
        }
        assert_eq!(EventOptions::default().validated_capacity(8).unwrap(), 8);
        assert_eq!(
            EventOptions::default()
                .methods(["Future.event", "Future.event"])
                .capacity(3)
                .validated_capacity(8)
                .unwrap(),
            3
        );
    }
}
