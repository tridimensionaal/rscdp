use super::Connection;
use crate::{Error, Result};
use std::time::Duration;
use tokio::{sync::Semaphore, time::Instant};

/// Transport limits and initial per-handle settings for a new connection.
///
/// Setters replace earlier values. All fields are validated before discovery or
/// network activity. Settings do not change an established connection; use
/// [`Connection::with_command_timeout`] to override a handle's command deadline.
/// Owned acquisition/cleanup bounds and private worker capacities are independent.
#[derive(Debug, Clone)]
#[must_use = "call connect().await to establish the configured connection"]
pub struct ConnectionOptions {
    /// Total HTTP discovery and WebSocket handshake budget; defaults to 30 seconds.
    /// Must be nonzero and representable as a monotonic deadline.
    pub connect_timeout: Duration,
    /// Socket send/flush budget, including pong flushes; defaults to 30 seconds.
    /// Must be nonzero. Incoming traffic does not extend an active write deadline.
    pub write_timeout: Duration,
    /// Initial caller deadline, including admission; defaults to Some(30 seconds).
    /// None disables this deadline, not transport or owned cleanup deadlines.
    /// Some(Duration::ZERO) does not guarantee that no command is sent.
    pub command_timeout: Option<Duration>,
    /// Client in-flight slots and request queue capacity; defaults to 256.
    /// Must be in 1..=tokio::sync::Semaphore::MAX_PERMITS. Cancellation releases
    /// a caller's slot but cannot recall work Chrome has already received.
    pub max_in_flight: usize,
    /// Default raw/typed event buffer capacity; defaults to 256.
    /// Must be in 1..=usize::MAX / 2. Tokio rounds up to a power of two.
    /// Counts events, not bytes; valid large values may still exhaust memory.
    pub event_capacity: usize,
    /// Incoming message and frame size limit in bytes; defaults to 100 MiB.
    /// Must be in 1..=isize::MAX. This is not a decoded JSON memory budget.
    pub max_message_size: usize,
}

impl Default for ConnectionOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            write_timeout: Duration::from_secs(30),
            command_timeout: Some(Duration::from_secs(30)),
            max_in_flight: 256,
            event_capacity: 256,
            max_message_size: 100 * 1024 * 1024,
        }
    }
}

impl ConnectionOptions {
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    pub fn write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = timeout;
        self
    }

    pub fn command_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.command_timeout = timeout;
        self
    }

    pub fn max_in_flight(mut self, capacity: usize) -> Self {
        self.max_in_flight = capacity;
        self
    }

    pub fn event_capacity(mut self, capacity: usize) -> Self {
        self.event_capacity = capacity;
        self
    }

    pub fn max_message_size(mut self, bytes: usize) -> Self {
        self.max_message_size = bytes;
        self
    }

    /// Connect to a ws/wss endpoint or discover it via HTTP(S) `/json/version`.
    /// The total connect budget starts when this future is polled.
    pub async fn connect(self, endpoint: &str) -> Result<Connection> {
        let deadline = deadline(self.connect_timeout, "connect_timeout")?;
        self.validate()?;
        tokio::time::timeout_at(deadline, Connection::connect_inner(endpoint, self))
            .await
            .map_err(|_| Error::Timeout("connecting to CDP".into()))?
    }

    fn validate(&self) -> Result<()> {
        for (name, duration) in [
            ("connect_timeout", self.connect_timeout),
            ("write_timeout", self.write_timeout),
        ] {
            if duration.is_zero() {
                return Err(Error::InvalidArgument(format!("{name} must be nonzero")));
            }
            deadline(duration, name)?;
        }
        if let Some(duration) = self.command_timeout {
            deadline(duration, "command_timeout")?;
        }
        for (name, value, maximum) in [
            ("max_in_flight", self.max_in_flight, Semaphore::MAX_PERMITS),
            ("event_capacity", self.event_capacity, usize::MAX / 2),
            (
                "max_message_size",
                self.max_message_size,
                isize::MAX as usize,
            ),
        ] {
            if value == 0 || value > maximum {
                return Err(Error::InvalidArgument(format!(
                    "{name} must be in 1..={maximum}"
                )));
            }
        }
        Ok(())
    }
}

pub(super) fn deadline(duration: Duration, name: &str) -> Result<Instant> {
    Instant::now().checked_add(duration).ok_or_else(|| {
        Error::InvalidArgument(format!(
            "{name} exceeds the supported monotonic deadline range"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_limits_and_deadlines_are_rejected_without_allocating_channels() {
        for options in [
            ConnectionOptions::default().connect_timeout(Duration::ZERO),
            ConnectionOptions::default().connect_timeout(Duration::MAX),
            ConnectionOptions::default().write_timeout(Duration::ZERO),
            ConnectionOptions::default().write_timeout(Duration::MAX),
            ConnectionOptions::default().command_timeout(Some(Duration::MAX)),
            ConnectionOptions::default().max_in_flight(0),
            ConnectionOptions::default().max_in_flight(Semaphore::MAX_PERMITS + 1),
            ConnectionOptions::default().event_capacity(0),
            ConnectionOptions::default().event_capacity(usize::MAX),
            ConnectionOptions::default().max_message_size(0),
            ConnectionOptions::default().max_message_size(usize::MAX),
        ] {
            assert!(matches!(options.validate(), Err(Error::InvalidArgument(_))));
        }
    }

    #[test]
    fn command_deadline_can_be_disabled_or_zero_and_event_capacity_can_be_unrounded() {
        ConnectionOptions::default().validate().unwrap();
        ConnectionOptions::default()
            .command_timeout(None)
            .event_capacity(3)
            .validate()
            .unwrap();
        ConnectionOptions::default()
            .command_timeout(Some(Duration::ZERO))
            .validate()
            .unwrap();
    }
}
