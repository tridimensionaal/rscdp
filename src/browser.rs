use crate::{
    Connection, Page, Result,
    cdp::{browser::BrowserContextId, target::TargetId},
    launcher::{BrowserProcess, LaunchOptions},
};
use serde_json::{Value, json};

/// A browser connection, its initial page, and optionally an owned Chrome process.
pub struct Browser {
    connection: Connection,
    page: Page,
    process: Option<BrowserProcess>,
}

impl Browser {
    pub async fn launch(options: LaunchOptions) -> Result<Self> {
        let (mut process, connection) = BrowserProcess::launch(options).await?;
        match Self::initial_page(&connection).await {
            Ok(page) => Ok(Self {
                connection,
                page,
                process: Some(process),
            }),
            Err(error) => {
                connection.close().await;
                process.close().await?;
                Err(error)
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
            create_page(connection, None).await
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
        let page_result = if let Some(lease) = &self.page.target_lease {
            lease.close().await
        } else {
            Ok(())
        };
        self.connection.close().await;
        if let Some(process) = &mut self.process {
            process.close().await?;
        }
        page_result
    }
}

pub struct BrowserContext {
    connection: Connection,
    id: Option<BrowserContextId>,
    page: Page,
}

impl BrowserContext {
    pub(crate) async fn create(connection: &Connection, params: Value) -> Result<Self> {
        let connection = connection.clone();
        // Finish acquiring ownership even if the caller cancels. An unclaimed
        // result is dropped by Tokio and disposes its context.
        tokio::spawn(async move { Self::create_owned(&connection, params).await }).await?
    }

    async fn create_owned(connection: &Connection, params: Value) -> Result<Self> {
        let response = connection
            .execute_raw("Target.createBrowserContext", params)
            .await?;
        let id: BrowserContextId = serde_json::from_value(response["browserContextId"].clone())?;
        match create_page(connection, Some(id.clone())).await {
            Ok(page) => Ok(Self {
                connection: connection.clone(),
                id: Some(id),
                page,
            }),
            Err(error) => {
                connection
                    .execute_raw(
                        "Target.disposeBrowserContext",
                        json!({"browserContextId":id}),
                    )
                    .await?;
                Err(error)
            }
        }
    }

    pub fn id(&self) -> Option<&BrowserContextId> {
        self.id.as_ref()
    }
    pub fn page(&self) -> &Page {
        &self.page
    }

    pub async fn close(&mut self) -> Result<()> {
        if let Some(id) = &self.id {
            self.connection
                .execute_raw(
                    "Target.disposeBrowserContext",
                    json!({"browserContextId":id}),
                )
                .await?;
            self.id = None;
        }
        Ok(())
    }
}

impl Drop for BrowserContext {
    fn drop(&mut self) {
        if let Some(id) = self.id.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let connection = self.connection.clone();
            runtime.spawn(async move {
                let _ = connection
                    .execute_raw(
                        "Target.disposeBrowserContext",
                        json!({"browserContextId":id}),
                    )
                    .await;
            });
        }
    }
}

async fn create_page(connection: &Connection, context: Option<BrowserContextId>) -> Result<Page> {
    let mut params = json!({"url":"about:blank"});
    if let Some(id) = &context {
        params["browserContextId"] = serde_json::to_value(id)?;
    }
    let response = connection
        .execute_raw("Target.createTarget", params)
        .await?;
    let target: TargetId = serde_json::from_value(response["targetId"].clone())?;
    let mut lease = TargetLease {
        connection: connection.clone(),
        target: Some(target.clone()),
        closed: std::sync::atomic::AtomicBool::new(false),
    };
    let result =
        async { Page::from_session(connection.attach(target.clone()).await?, context).await }.await;
    match result {
        Ok(mut page) => {
            page.target_lease = Some(std::sync::Arc::new(lease));
            Ok(page)
        }
        Err(error) => {
            connection
                .execute_raw("Target.closeTarget", json!({"targetId":target}))
                .await?;
            lease.target = None;
            Err(error)
        }
    }
}

pub(crate) struct TargetLease {
    connection: Connection,
    target: Option<TargetId>,
    closed: std::sync::atomic::AtomicBool,
}

impl TargetLease {
    async fn close(&self) -> Result<()> {
        if !self.closed.load(std::sync::atomic::Ordering::Acquire)
            && let Some(target) = &self.target
        {
            self.connection
                .execute_raw("Target.closeTarget", json!({"targetId":target}))
                .await?;
            self.closed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        Ok(())
    }
}

impl Drop for TargetLease {
    fn drop(&mut self) {
        if !self.closed.load(std::sync::atomic::Ordering::Acquire)
            && let Some(target) = self.target.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let connection = self.connection.clone();
            runtime.spawn(async move {
                let _ = connection
                    .execute_raw("Target.closeTarget", json!({"targetId":target}))
                    .await;
            });
        }
    }
}
