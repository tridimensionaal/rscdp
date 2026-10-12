use crate::{Error, Result};
use std::time::Instant;

// Never format params, results, endpoints or error text in diagnostics.
pub(crate) struct Command<'a> {
    method: &'a str,
    started: Instant,
    outcome: &'static str,
}

impl<'a> Command<'a> {
    pub(crate) fn new(method: &'a str) -> Self {
        Self {
            method,
            started: Instant::now(),
            outcome: "cancelled",
        }
    }

    pub(crate) fn finish<T>(&mut self, result: &Result<T>) {
        self.outcome = match result {
            Ok(_) => "ok",
            Err(Error::Timeout(_)) => "timeout",
            Err(Error::SessionClosed(_)) => "session_closed",
            Err(Error::ConnectionClosed(_)) => "connection_closed",
            Err(Error::ConnectionFailed(_)) => "connection_failed",
            Err(Error::Protocol(_)) => "protocol_error",
            Err(_) => "error",
        };
    }
}

impl Drop for Command<'_> {
    fn drop(&mut self) {
        tracing::debug!(target: "rustcdp", method = self.method,
            elapsed_ms = self.started.elapsed().as_secs_f64() * 1000.0,
            outcome = self.outcome, "command finished");
    }
}
