use super::{Browser, BrowserContext};
use crate::{
    Error, Page, Result,
    error::with_cleanup,
    launcher::{LaunchOptions, args::validated_switch_name},
    proxy::{Proxy, ProxyAuth, ProxyAuthStatus},
};
use serde_json::{Value, json};
use std::time::Duration;

impl Browser {
    /// Launch with a process-wide proxy and optional credential worker.
    /// Credentials arm authentication on an initial page with known inherited
    /// routing before returning. A foreign context's unknown routing receives no
    /// inferred credentials. Setup does not navigate;
    /// navigate to your intended destination, then use [`Self::wait_for_proxy_ready`]
    /// to observe authentication. Without credentials, no worker is started.
    /// The worker handles only the initial page's session; other tabs and separate
    /// child sessions do not automatically receive authentication workers.
    /// The supplied configuration is retained after successful setup so
    /// [`Self::new_context`] can authenticate its managed pages while inheriting
    /// the process routing and bypass rules unchanged.
    /// Setup continues in an owned task after cancellation. An unclaimed browser
    /// is dropped and its owned process is shut down.
    /// Raw `proxy-server`, `no-proxy-server`, `proxy-pac-url`, and
    /// `proxy-auto-detect` switches conflict with the explicit proxy and are
    /// rejected. `proxy-bypass-list` customization is allowed.
    pub async fn launch_with_proxy(mut options: LaunchOptions, proxy: Proxy) -> Result<Self> {
        for arg in &options.args {
            if matches!(
                validated_switch_name(arg)?.as_deref(),
                Some("proxy-server" | "no-proxy-server" | "proxy-pac-url" | "proxy-auto-detect")
            ) {
                return Err(Error::InvalidArgument(
                    "raw proxy routing switch conflicts with the explicit proxy".into(),
                ));
            }
        }
        options
            .args
            .push(format!("--proxy-server={}", proxy.server()).into());
        tokio::spawn(async move {
            let mut browser = Self::launch(options).await?;
            let auth = if browser.pages.initial_uses_process_proxy() {
                start_auth(&browser.page, proxy.clone()).await
            } else {
                // An existing foreign context can override process routing.
                Ok(None)
            };
            match auth {
                Ok(auth) => {
                    browser.page.auth = auth;
                    browser
                        .pages
                        .set_process_proxy(&browser.page, proxy.clone());
                    browser.process_proxy = Some(proxy);
                    Ok(browser)
                }
                Err(error) => with_cleanup(Err(error), browser.close().await),
            }
        })
        .await?
    }

    /// Create an isolated context with its own proxy configuration.
    /// Context acquisition follows [`BrowserContext`]'s ownership contract.
    /// Credentials arm authentication before returning; setup does not navigate.
    /// Without credentials, no worker is started. Each managed page, including
    /// those created by BrowserContext::new_page, receives its own worker.
    /// Popups and separate child sessions are not automatically authenticated.
    /// Inspect [`BrowserContext::wait_for_proxy_ready`] after navigating to your
    /// intended destination; readiness alone does not prove proxy traversal.
    ///
    /// The caller's command deadline limits waiting. Context acquisition and
    /// authentication setup each have their own 30-second bounds and continue
    /// after cancellation. Unclaimed contexts are disposed after setup finishes.
    pub async fn new_context_with_proxy(&self, proxy: Proxy) -> Result<BrowserContext> {
        self.create_context_with_auth(
            json!({"disposeOnDetach":true,"proxyServer":proxy.server()}),
            Some(proxy),
        )
        .await
    }

    pub(super) async fn create_context_with_auth(
        &self,
        params: Value,
        proxy: Option<Proxy>,
    ) -> Result<BrowserContext> {
        self.pages.ensure_open()?;
        let timeout = self.connection.command_timeout();
        let connection = self.connection.clone();
        let pages = self.pages.clone();
        let task = connection.runtime().spawn(async move {
            let _acquisition =
                tokio::time::timeout(Duration::from_secs(30), pages.acquisition_gate.lock())
                    .await
                    .map_err(|_| Error::Timeout("waiting to create browser context".into()))?;
            pages.ensure_open()?;
            // One owner retains the context through authentication setup.
            let mut context = BrowserContext::create_owned(&connection, params).await?;
            // Await bounded auth setup/recovery in this owner before disposal;
            // the caller's timeout only stops waiting for the owned task.
            let auth = match proxy.as_ref() {
                Some(proxy) => start_auth(&context.page, proxy.clone()).await,
                None => Ok(None),
            };
            match auth {
                Ok(auth) => {
                    context.page.auth = auth;
                    context
                        .lease
                        .as_ref()
                        .expect("context owns its lease")
                        .register_page(&context.page, proxy);
                    let registered = pages
                        .register_context(context.lease.as_ref().expect("context owns its lease"));
                    match registered {
                        Ok(()) => Ok(context),
                        Err(error) => with_cleanup(Err(error), context.close().await),
                    }
                }
                Err(error) => with_cleanup(Err(error), context.close().await),
            }
        });
        if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("creating browser context".into()))??
        } else {
            task.await?
        }
    }

    /// Worker status, or `None` when no credential worker is configured.
    pub fn proxy_auth_status(&self) -> Option<ProxyAuthStatus> {
        self.page.proxy_auth_status()
    }

    /// Wait for a successful request without an observed authentication failure.
    /// Returns `None` without a credential worker, otherwise the observed
    /// `Authenticated` or `Unchallenged` status. Readiness does not prove proxy
    /// traversal or that supplied credentials were used.
    pub async fn wait_for_proxy_ready(&self, timeout: Duration) -> Result<Option<ProxyAuthStatus>> {
        self.page.wait_for_proxy_ready(timeout).await
    }

    /// Stop the initial page's authentication worker and await Fetch cleanup.
    /// Canceling the wait leaves cleanup running; call again to observe completion.
    /// The proxy route remains configured, so later requests may require credentials.
    pub async fn stop_proxy_auth(&self) -> Result<()> {
        self.page.stop_proxy_auth().await
    }
}

impl BrowserContext {
    /// Worker status, or `None` when no credential worker is configured.
    pub fn proxy_auth_status(&self) -> Option<ProxyAuthStatus> {
        self.page.proxy_auth_status()
    }

    /// Wait for a successful request without an observed authentication failure.
    /// Returns `None` without a credential worker, otherwise the observed
    /// `Authenticated` or `Unchallenged` status. Readiness does not prove proxy
    /// traversal or that supplied credentials were used.
    pub async fn wait_for_proxy_ready(&self, timeout: Duration) -> Result<Option<ProxyAuthStatus>> {
        self.page.wait_for_proxy_ready(timeout).await
    }

    /// Stop the initial page's authentication worker and await Fetch cleanup.
    /// Canceling the wait leaves cleanup running; call again to observe completion.
    /// The proxy route remains configured, so later requests may require credentials.
    pub async fn stop_proxy_auth(&self) -> Result<()> {
        self.page.stop_proxy_auth().await
    }
}

pub(super) async fn start_auth(
    page: &Page,
    proxy: Proxy,
) -> Result<Option<std::sync::Arc<ProxyAuth>>> {
    if proxy.requires_authentication() {
        ProxyAuth::start(page.session().clone(), proxy)
            .await
            .map(|auth| Some(std::sync::Arc::new(auth)))
    } else {
        Ok(None)
    }
}
