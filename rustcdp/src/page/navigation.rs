use super::Page;
use crate::{
    Error, Result,
    cdp::page::{
        commands::{GetNavigationHistory, NavigateResponse},
        events::{FrameNavigated, LifecycleEvent},
    },
};
use serde_json::json;
use std::time::Duration;

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
