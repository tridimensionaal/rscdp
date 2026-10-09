use rustcdp::{
    Connection, Result,
    cdp::{page, target},
};
use serde_json::json;
use std::time::Duration;

async fn navigate(connection: &Connection) -> Result<()> {
    let target = connection
        .execute_raw("Target.createTarget", json!({"url":"about:blank"}))
        .await?;
    let target_id: target::TargetId = serde_json::from_value(target["targetId"].clone())?;
    let session = connection.attach(target_id.clone()).await?;
    session.execute(&page::commands::Enable::default()).await?;
    let mut loaded = session.listen::<page::events::LoadEventFired>();
    session
        .execute_raw(
            "Page.navigate",
            json!({"url":"data:text/html,<title>rustcdp</title><h1>Hello</h1>"}),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(10), loaded.recv())
        .await
        .map_err(|_| rustcdp::Error::Timeout("page load".into()))??;
    let title = session
        .execute_raw(
            "Runtime.evaluate",
            json!({"expression":"document.title","returnByValue":true}),
        )
        .await?;
    println!("{}", title["result"]["value"]);
    session.detach().await?;
    connection
        .execute_raw("Target.closeTarget", json!({"targetId":target_id}))
        .await?;
    Ok(())
}

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let endpoint = std::env::args()
        .nth(1)
        .ok_or("usage: session <debugging URL>")?;
    let connection = Connection::connect(&endpoint).await?;
    let result = navigate(&connection).await;
    connection.close().await;
    result?;
    Ok(())
}
