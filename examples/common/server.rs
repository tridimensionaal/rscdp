//! Serve the example pages over HTTP when cookies or fetch need a real origin.
use axum::Router;
use rscdp::Result;
use std::path::Path;
use tokio::{net::TcpListener, task::JoinHandle};
use tower_http::services::ServeDir;

pub struct Server {
    origin: String,
    task: JoinHandle<std::io::Result<()>>,
}

impl Server {
    pub async fn start() -> Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let origin = format!("http://{}", listener.local_addr()?);
        let pages = Path::new(env!("CARGO_MANIFEST_DIR")).join("pages");
        let app = Router::new().fallback_service(ServeDir::new(pages));
        let task = tokio::spawn(async move { axum::serve(listener, app).await });
        Ok(Self { origin, task })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}/{}", self.origin, path.trim_start_matches('/'))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
