use rustcdp::{Connection, cdp::browser::commands::GetVersion};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = std::env::args()
        .nth(1)
        .ok_or("usage: version <debugging URL>")?;
    let connection = Connection::connect(&endpoint).await?;
    let result = connection.execute(&GetVersion::default()).await;
    connection.close().await;
    println!("{}", result?.product);
    Ok(())
}
