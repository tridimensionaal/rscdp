use crate::{
    Connection, Error, Page, Result,
    cdp::{browser::BrowserContextId, target::TargetId},
    error::with_cleanup,
    launcher::{BrowserProcess, LaunchOptions},
};
use serde_json::{Value, json};

mod proxy;
mod resource;
use resource::ContextLease;
pub(crate) use resource::TargetLease;

/// A browser connection, its initial page, and optionally an owned Chrome process.
pub struct Browser {
    connection: Connection,
    page: Page,
    process: Option<BrowserProcess>,
    auth: Option<crate::proxy_auth::ProxyAuth>,
}

impl Browser {
    pub async fn launch(options: LaunchOptions) -> Result<Self> {
        let (mut process, connection) = BrowserProcess::launch(options).await?;
        match Self::initial_page(&connection).await {
            Ok(page) => Ok(Self {
                connection,
                page,
                process: Some(process),
                auth: None,
            }),
            Err(error) => {
                connection.close().await;
                with_cleanup(Err(error), process.close().await)
            }
        }
    }

    /// Connect to an existing browser. Closing this handle disconnects without killing it.
    pub async fn connect(endpoint: &str) -> Result<Self> {
        let connection = Connection::connect(endpoint).await?;
        match Self::initial_page(&connection).await {
            Ok(page) => Ok(Self {
                connection,
                page,
                process: None,
                auth: None,
            }),
            Err(error) => {
                connection.close().await;
                Err(error)
            }
        }
    }

    async fn initial_page(connection: &Connection) -> Result<Page> {
        let targets = connection
            .execute(&crate::cdp::target::commands::GetTargets::default())
            .await?;
        if let Some(target) = targets
            .target_infos
            .into_iter()
            .find(|target| target.type_ == "page")
        {
            // Chrome also reports an ID for the default context, but Storage
            // accepts explicit IDs only for contexts returned by this command.
            let contexts = connection
                .execute(&crate::cdp::target::commands::GetBrowserContexts::default())
                .await?;
            let context = target
                .browser_context_id
                .filter(|id| contexts.browser_context_ids.contains(id));
            let session = connection.attach(target.target_id).await?;
            Page::from_session(session, context).await
        } else {
            create_fallback_page(connection).await
        }
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }
    pub fn page(&self) -> &Page {
        &self.page
    }
    pub fn process(&self) -> Option<&BrowserProcess> {
        self.process.as_ref()
    }

    pub async fn new_context(&self) -> Result<BrowserContext> {
        BrowserContext::create(
            &self.connection,
            json!({"disposeOnDetach":true,"proxyServer":"","proxyBypassList":"*"}),
        )
        .await
    }

    pub async fn close(&mut self) -> Result<()> {
        let auth_result = if let Some(auth) = &mut self.auth {
            auth.stop().await
        } else {
            Ok(())
        };
        let page_result = if let Some(lease) = &self.page.target_lease {
            lease.close().await
        } else {
            Ok(())
        };
        self.connection.close().await;
        let process_result = if let Some(process) = &mut self.process {
            process.close().await
        } else {
            Ok(())
        };
        with_cleanup(with_cleanup(auth_result, page_result), process_result)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.detach_proxy_auth();
    }
}

pub struct BrowserContext {
    lease: ContextLease,
    page: Page,
    auth: Option<crate::proxy_auth::ProxyAuth>,
}

impl BrowserContext {
    pub(crate) async fn create(connection: &Connection, params: Value) -> Result<Self> {
        let timeout = connection.command_timeout();
        let connection = connection.clone();
        // Finish acquiring ownership even if the caller cancels. An unclaimed
        // result is dropped by Tokio and disposes its context.
        let task = connection
            .runtime()
            .spawn(async move { Self::create_owned(&connection, params).await });
        let joined = if let Some(timeout) = timeout {
            tokio::time::timeout(timeout, task)
                .await
                .map_err(|_| Error::Timeout("creating browser context".into()))?
        } else {
            task.await
        };
        joined?
    }

    async fn create_owned(connection: &Connection, mut params: Value) -> Result<Self> {
        // Disconnection must resolve ownership even if the creation reply is lost.
        params["disposeOnDetach"] = true.into();
        let commands = connection.clone().with_command_timeout(None);
        let mut lease = None;
        let result = tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let response = commands
                .execute_raw("Target.createBrowserContext", params)
                .await?;
            let id: BrowserContextId =
                serde_json::from_value(response["browserContextId"].clone())?;
            lease = Some(ContextLease::new(connection.clone(), id.clone()));
            // The context guard owns every resource acquired here. Keep setup in
            // this task so disposal cannot be followed by a nested task's recovery.
            let target = commands
                .execute(
                    &crate::cdp::target::commands::CreateTarget::new("about:blank")
                        .with_browser_context_id(id.clone()),
                )
                .await?
                .target_id;
            let attached = commands
                .execute(
                    &crate::cdp::target::commands::AttachToTarget::new(target.clone())
                        .with_flatten(true),
                )
                .await?;
            let session = commands.session(attached.session_id, target)?;
            Page::from_session_in_context(session, id).await
        })
        .await
        .unwrap_or_else(|_| Err(Error::Timeout("creating browser context".into())));
        match result {
            Ok(page) => Ok(Self {
                lease: lease.expect("context ownership precedes page creation"),
                page: page.with_command_timeout(connection.command_timeout()),
                auth: None,
            }),
            Err(error) => {
                if let Some(lease) = lease {
                    let cleanup = lease.close().await;
                    if cleanup.is_err() {
                        connection.close().await;
                    }
                    with_cleanup(Err(error), cleanup)
                } else {
                    // A protocol rejection confirms that creation failed.
                    // Other failures can leave an unknown context on the server.
                    if !matches!(&error, Error::Protocol(_)) {
                        connection.close().await;
                    }
                    Err(error)
                }
            }
        }
    }

    /// The context ID, or `None` after disposal has been acknowledged.
    pub fn id(&self) -> Option<&BrowserContextId> {
        self.lease.id()
    }
    pub fn page(&self) -> &Page {
        &self.page
    }

    /// Stop authentication and dispose this context in an owned runtime task.
    /// Cancellation does not stop cleanup. Disposal uses the configured command
    /// timeout, capped at 30 seconds for the disposal command. An uncertain
    /// timeout closes the connection; protocol errors remain retryable.
    pub async fn close(&mut self) -> Result<()> {
        self.lease.close_with_auth(self.auth.take()).await?
    }
}

impl Drop for BrowserContext {
    fn drop(&mut self) {
        self.lease.register_auth(self.auth.take());
    }
}

async fn create_fallback_page(connection: &Connection) -> Result<Page> {
    let response = connection
        .execute_raw("Target.createTarget", json!({"url":"about:blank"}))
        .await?;
    let target: TargetId = serde_json::from_value(response["targetId"].clone())?;
    let lease = TargetLease::new(connection.clone(), target.clone());
    let result =
        async { Page::from_session(connection.attach(target.clone()).await?, None).await }.await;
    match result {
        Ok(mut page) => {
            page.target_lease = Some(std::sync::Arc::new(lease));
            Ok(page)
        }
        Err(error) => with_cleanup(Err(error), lease.close().await),
    }
}
