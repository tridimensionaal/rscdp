use rustcdp::{
    cdp::browser::commands::GetVersion,
    launcher::{BrowserProcess, LaunchOptions},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = LaunchOptions {
        args: std::iter::once("--headless=new".into())
            .chain(std::env::args_os().skip(1))
            .collect(),
        dir_prefix: Some("rustcdp-example-".into()),
        ..Default::default()
    };
    let (mut browser, connection) = BrowserProcess::launch(options).await?;
    let result = connection.execute(&GetVersion::default()).await;
    connection.close().await;
    browser.close().await?;
    println!("{}", result?.product);
    Ok(())
}
