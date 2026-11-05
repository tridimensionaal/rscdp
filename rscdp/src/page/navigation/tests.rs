use super::*;
use crate::{Connection, cdp::target::TargetId, page::TargetKind};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

struct Peer {
    page: Page,
    task: JoinHandle<()>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Peer {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = match request["method"].as_str().unwrap() {
                    "Target.attachToTarget" => json!({"sessionId":"session"}),
                    "Page.getFrameTree" => {
                        json!({"frameTree":{"frame":frame("selected","baseline")}})
                    }
                    "Test.emit" => {
                        for event in request["params"]["events"].as_array().unwrap() {
                            socket
                                .send(Message::Text(event.to_string().into()))
                                .await
                                .unwrap();
                        }
                        json!({})
                    }
                    "Page.enable" | "Runtime.enable" | "Page.setLifecycleEventsEnabled" => {
                        json!({})
                    }
                    method => panic!("unexpected peer command: {method}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .unwrap();
            }
        });
        let connection = Connection::connect(&endpoint)
            .await
            .unwrap()
            .with_command_timeout(Some(Duration::from_secs(2)));
        let session = connection.attach(TargetId("iframe".into())).await.unwrap();
        let page = Page::from_session(session, None, TargetKind::Iframe)
            .await
            .unwrap();
        Self { page, task }
    }

    async fn emit(&self, events: Vec<Value>) {
        // Reply follows all events on the same socket, so their delivery precedes
        // the returned command result. No sleeps or timing guesses are needed.
        self.page
            .session
            .execute_raw("Test.emit", json!({"events":events}))
            .await
            .unwrap();
    }
}

fn frame(id: &str, loader: &str) -> Value {
    json!({"id":id,"parentId":"parent","loaderId":loader,"url":"http://fixture.test/next",
        "domainAndRegistry":"fixture.test","securityOrigin":"http://fixture.test",
        "mimeType":"text/html","secureContextType":"InsecureScheme",
        "crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]})
}

fn commit(id: &str, loader: &str) -> Value {
    json!({"sessionId":"session","method":"Page.frameNavigated","params":{"frame":frame(id,loader),"type":"Navigation"}})
}

fn load(id: &str, loader: &str) -> Value {
    json!({"sessionId":"session","method":"Page.lifecycleEvent","params":{"frameId":id,"loaderId":loader,"name":"load","timestamp":1.0}})
}

#[tokio::test]
async fn observer_ignores_descendant_and_old_loader_events() {
    let peer = Peer::new().await;
    let observer = peer.page.expect_navigation().prepare().await.unwrap();
    peer.emit(vec![
        commit("descendant", "child"),
        load("descendant", "child"),
        commit("selected", "baseline"),
        load("selected", "baseline"),
    ])
    .await;
    let wait = observer.wait();
    tokio::pin!(wait);
    tokio::select! {
        biased;
        result=&mut wait => panic!("unrelated documents satisfied observer: {result:?}"),
        _=std::future::ready(())=>{},
    }
    peer.emit(vec![
        commit("selected", "next"),
        load("descendant", "next"),
        load("selected", "baseline"),
    ])
    .await;
    tokio::select! {
        biased;
        result=&mut wait => panic!("wrong lifecycle satisfied observer: {result:?}"),
        _=std::future::ready(())=>{},
    }
    peer.emit(vec![load("selected", "next")]).await;
    let result = tokio::time::timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.frame_id.0, "selected");
    assert_eq!(result.loader_id.unwrap().0, "next");
}

#[tokio::test]
async fn observer_rejects_empty_loader_and_selected_frame_detachment() {
    let peer = Peer::new().await;
    for event in [
        commit("selected", ""),
        json!({"sessionId":"session","method":"Page.frameDetached","params":{"frameId":"selected","reason":"remove"}}),
    ] {
        let observer = peer.page.expect_navigation().prepare().await.unwrap();
        peer.emit(vec![event]).await;
        assert!(matches!(observer.wait().await, Err(Error::Navigation(_))));
    }
}

#[tokio::test]
async fn observer_rejects_supersession_before_load() {
    let peer = Peer::new().await;
    let observer = peer.page.expect_navigation().prepare().await.unwrap();
    peer.emit(vec![
        commit("selected", "one"),
        commit("selected", "two"),
        load("selected", "one"),
    ])
    .await;
    assert!(
        matches!(observer.wait().await,Err(Error::Navigation(message)) if message.contains("superseded"))
    );
}

#[tokio::test]
async fn observer_preserves_deadline_and_lag_errors() {
    let peer = Peer::new().await;
    let mut observer = peer
        .page
        .expect_navigation()
        .wait_until(WaitUntil::Commit)
        .prepare()
        .await
        .unwrap();
    peer.emit(vec![commit("selected", "next")]).await;
    observer.deadline = Instant::now();
    assert!(matches!(observer.wait().await, Err(Error::Timeout(_))));

    let mut observer = peer.page.expect_navigation().prepare().await.unwrap();
    observer.events = peer
        .page
        .session
        .subscribe_with(
            EventOptions::default()
                .methods(OBSERVATION_METHODS)
                .capacity(2),
        )
        .unwrap();
    peer.emit(vec![
        commit("selected", "next"),
        load("selected", "next"),
        load("selected", "next"),
    ])
    .await;
    assert!(matches!(observer.wait().await, Err(Error::Lagged(_))));
}
