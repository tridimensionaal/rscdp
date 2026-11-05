use super::Page;
use crate::{Result, proxy::ProxyAuthStatus};
use std::time::Duration;

impl Page {
    /// Status of this page's managed proxy worker, or None if none was configured.
    /// Page clones share the worker and its status, including after it stops.
    pub fn proxy_auth_status(&self) -> Option<ProxyAuthStatus> {
        self.auth.as_ref().map(|auth| auth.status())
    }

    /// Observe authenticated or unchallenged traffic on this page's session.
    /// Returns None when no worker was configured. Readiness does not prove proxy
    /// traversal or that credentials were used; it does not cover other targets.
    pub async fn wait_for_proxy_ready(&self, timeout: Duration) -> Result<Option<ProxyAuthStatus>> {
        match &self.auth {
            Some(auth) => auth.wait_ready(timeout).await.map(Some),
            None => Ok(None),
        }
    }

    /// Stop this page's managed worker and await its Fetch cleanup/recovery.
    /// All clones observe the same stopped worker. Canceling the wait leaves
    /// cleanup running; another call can await it. Errors are returned once.
    /// The proxy route remains configured, so later requests may need credentials.
    /// See [`crate::proxy::auth`] for failure recovery and its affected resources.
    pub async fn stop_proxy_auth(&self) -> Result<()> {
        if let Some(auth) = &self.auth {
            auth.stop().await?;
        }
        Ok(())
    }
}
