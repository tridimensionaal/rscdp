use super::{Browser, BrowserContext};
use crate::{
    Error, Page, Result,
    launcher::LaunchOptions,
    proxy::{Proxy, ProxyAuthMode},
    proxy_auth::{ProxyAuth, ProxyAuthStatus},
};
use serde_json::json;
use std::time::Duration;

impl Browser {
    pub async fn launch_with_proxy(
        mut options: LaunchOptions,
        proxy: Proxy,
        mode: ProxyAuthMode,
    ) -> Result<Self> {
        if options
            .args
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with("--proxy-server"))
        {
            return Err(Error::InvalidArgument(
                "proxy already specified in launch arguments".into(),
            ));
        }
        options
            .args
            .push(format!("--proxy-server={}", proxy.server()).into());
        let mut browser = Self::launch(options).await?;
        match start_auth(&browser.page, proxy, mode).await {
            Ok(auth) => {
                browser.auth = auth;
                Ok(browser)
            }
            Err(error) => {
                browser.close().await?;
                Err(error)
            }
        }
    }

    pub async fn new_context_with_proxy(
        &self,
        proxy: Proxy,
        mode: ProxyAuthMode,
    ) -> Result<BrowserContext> {
        let mut context = BrowserContext::create(
            &self.connection,
            json!({"disposeOnDetach":true,"proxyServer":proxy.server()}),
        )
        .await?;
        match start_auth(&context.page, proxy, mode).await {
            Ok(auth) => {
                context.auth = auth;
                Ok(context)
            }
            Err(error) => {
                context.close().await?;
                Err(error)
            }
        }
    }

    pub fn proxy_auth_status(&self) -> Option<ProxyAuthStatus> {
        self.auth.as_ref().map(ProxyAuth::status)
    }

    /// Wait for request readiness; inspect `proxy_auth_status()` for authentication evidence.
    pub async fn wait_for_proxy_ready(&self, timeout: Duration) -> Result<()> {
        if let Some(auth) = &self.auth {
            auth.wait_ready(timeout).await?;
        }
        Ok(())
    }

    /// Cancel authentication without waiting for its epilogue, immediately before close.
    pub fn detach_proxy_auth(&mut self) {
        if let Some(mut auth) = self.auth.take() {
            auth.detach();
        }
    }
}

impl BrowserContext {
    pub fn proxy_auth_status(&self) -> Option<ProxyAuthStatus> {
        self.auth.as_ref().map(ProxyAuth::status)
    }

    /// Wait for request readiness; inspect `proxy_auth_status()` for authentication evidence.
    pub async fn wait_for_proxy_ready(&self, timeout: Duration) -> Result<()> {
        if let Some(auth) = &self.auth {
            auth.wait_ready(timeout).await?;
        }
        Ok(())
    }

    /// Use immediately before closing an unresponsive context.
    pub fn detach_proxy_auth(&mut self) {
        if let Some(mut auth) = self.auth.take() {
            auth.detach();
        }
    }
}

async fn start_auth(page: &Page, proxy: Proxy, mode: ProxyAuthMode) -> Result<Option<ProxyAuth>> {
    if !proxy.requires_authentication() {
        return Ok(None);
    }
    let auth = ProxyAuth::start(page.session().clone(), proxy).await?;
    if mode == ProxyAuthMode::Preflight {
        tokio::time::timeout(Duration::from_secs(15), async {
            page.navigate("https://www.example.com/").await?;
            auth.wait_ready(Duration::from_secs(15)).await
        })
        .await
        .map_err(|_| Error::Timeout("proxy authentication preflight".into()))??;
    }
    Ok(Some(auth))
}
