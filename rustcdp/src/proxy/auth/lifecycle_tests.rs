//! Lifecycle tests use a real connection and session against a scripted CDP peer.
use super::{ProxyAuth, ProxyAuthStatus};
use crate::{Connection, Error, Session, cdp::target::TargetId};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, time::Duration};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

const SESSION: &str = "auth-session";
const TARGET: &str = "auth-target";

async fn bounded(test: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(5), test)
        .await
        .expect("authentication lifecycle test timed out");
}

// The test owns the socket directly: a panic or timeout drops it, waking the
// real connection and its workers. There is no detached fake-peer task to leak.
struct Peer(WebSocketStream<TcpStream>);

impl Peer {
    async fn session() -> (Self, Session) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let (connection, socket) = tokio::join!(Connection::connect(&endpoint), async {
            let (socket, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_async(socket).await.unwrap()
        });
        let connection = connection.unwrap().with_command_timeout(None);
        let mut peer = Self(socket);
        let (session, ()) = tokio::join!(connection.attach(TargetId(TARGET.into())), async {
            let request = peer.request("Target.attachToTarget").await;
            assert_eq!(
                request["params"],
                json!({"targetId": TARGET, "flatten": true})
            );
            peer.reply(&request, json!({"sessionId": SESSION})).await;
        });
        (peer, session.unwrap())
    }

    async fn request(&mut self, method: &str) -> Value {
        let message = self.0.next().await.expect("peer EOF").expect("peer read");
        let request: Value =
            serde_json::from_str(message.to_text().expect("CDP text")).expect("CDP JSON");
        assert_eq!(request["method"], method, "unexpected request: {request}");
        let scope = if method.starts_with("Network.") || method.starts_with("Fetch.") {
            json!(SESSION)
        } else {
            Value::Null
        };
        assert_eq!(request["sessionId"], scope, "wrong command scope");
        request
    }

    async fn reply(&mut self, request: &Value, result: Value) {
        self.0
            .send(Message::Text(
                json!({"id": request["id"], "result": result})
                    .to_string()
                    .into(),
            ))
            .await
            .unwrap();
    }

    async fn fail(&mut self, request: &Value, message: &str) {
        self.0
            .send(Message::Text(
                json!({
                    "id": request["id"],
                    "error": {"code": -32000, "message": message}
                })
                .to_string()
                .into(),
            ))
            .await
            .unwrap();
    }

    async fn ok(&mut self, method: &str) {
        let request = self.request(method).await;
        self.reply(&request, json!({})).await;
    }

    async fn start(&mut self, session: &Session, enable_network: bool) -> ProxyAuth {
        let (auth, ()) = tokio::join!(start(session), async {
            if enable_network {
                self.ok("Network.enable").await;
            }
            let request = self.request("Fetch.enable").await;
            assert_eq!(request["params"]["handleAuthRequests"], true);
            self.reply(&request, json!({})).await;
        });
        let auth = auth.unwrap_or_else(|error| panic!("start failed: {error}"));
        assert_eq!(auth.status(), ProxyAuthStatus::Pending);
        auth
    }

    async fn recover_fetch(&mut self) {
        let close = self.request("Target.closeTarget").await;
        assert_eq!(close["params"], json!({"targetId": TARGET}));
        // Keep the session live so recovery must disable interception before
        // releasing Network, rather than relying on a detached target.
        self.reply(&close, json!({"success": false})).await;
        self.ok("Fetch.disable").await;
    }

    async fn finish(&mut self, session: &Session) {
        session.connection().close().await;
        let message = self.0.next().await.expect("close frame").unwrap();
        assert!(
            message.is_close(),
            "unexpected command after cleanup: {message:?}"
        );
    }
}

async fn start(session: &Session) -> crate::Result<ProxyAuth> {
    ProxyAuth::start(
        session.clone(),
        "http://user:password@proxy.invalid:8080".parse().unwrap(),
    )
    .await
}

// Drive a caller alongside the peer, rejecting completion before its cleanup
// command has even been acknowledged. No sleeps or scheduler assumptions.
async fn while_pending<F: Future, T>(caller: Pin<&mut F>, action: impl Future<Output = T>) -> T {
    tokio::select! {
        biased;
        _ = caller => panic!("caller completed before the expected peer action"),
        value = action => value,
    }
}

fn assert_protocol(error: &Error, expected: &str) {
    assert!(
        matches!(error, Error::Protocol(error) if error.message == expected),
        "{error:?}"
    );
}

#[tokio::test]
async fn canceled_start_awaits_network_acquisition_and_release_without_enabling_fetch() {
    bounded(async {
        let (mut peer, session) = Peer::session().await;
        let mut starting = Box::pin(start(&session));
        let enable = while_pending(starting.as_mut(), peer.request("Network.enable")).await;
        drop(starting);
        peer.reply(&enable, json!({})).await;

        // Fetch.enable here would leave interception armed for an absent caller.
        let disable = peer.request("Network.disable").await;
        // If the auth worker canceled its nested guard acquisition, that guard's
        // detached drop could send disable after the auth lease was abandoned.
        // Holding the disable reply makes this ownership assertion deterministic.
        let error = start(&session)
            .await
            .err()
            .expect("cleanup must retain the auth lease");
        assert!(matches!(error, Error::ProxyAuthentication(message)
            if message.contains("already armed")));
        peer.reply(&disable, json!({})).await;
        // Wait for the canceled worker to consume the reply and release its
        // lease before closing the socket with potentially unread data.
        loop {
            if let Ok(lease) = session.claim_proxy_auth() {
                drop(lease);
                break;
            }
            tokio::task::yield_now().await;
        }
        peer.finish(&session).await;
    })
    .await;
}

#[tokio::test]
async fn failed_fetch_enable_recovers_interception_before_releasing_network() {
    bounded(async {
        let (mut peer, session) = Peer::session().await;
        let mut starting = Box::pin(start(&session));
        while_pending(starting.as_mut(), peer.ok("Network.enable")).await;
        let enable = while_pending(starting.as_mut(), peer.request("Fetch.enable")).await;
        peer.fail(&enable, "Fetch.enable failed").await;
        while_pending(starting.as_mut(), peer.recover_fetch()).await;
        let disable = while_pending(starting.as_mut(), peer.request("Network.disable")).await;
        peer.reply(&disable, json!({})).await;
        let error = starting.await.err().expect("setup must fail");
        assert_protocol(&error, "Fetch.enable failed");
        peer.finish(&session).await;
    })
    .await;
}

#[tokio::test]
async fn normal_stop_waits_for_fetch_then_network_disable() {
    bounded(async {
        let (mut peer, session) = Peer::session().await;
        let auth = peer.start(&session, true).await;
        let mut stopping = Box::pin(auth.stop());
        while_pending(stopping.as_mut(), peer.ok("Fetch.disable")).await;
        let disable = while_pending(stopping.as_mut(), peer.request("Network.disable")).await;
        peer.reply(&disable, json!({})).await;
        stopping.await.unwrap();
        assert_eq!(auth.status(), ProxyAuthStatus::Stopped);
        auth.stop().await.unwrap();
        peer.finish(&session).await;
    })
    .await;
}

#[tokio::test]
async fn failed_fetch_disable_recovers_before_releasing_network() {
    bounded(async {
        let (mut peer, session) = Peer::session().await;
        let auth = peer.start(&session, true).await;
        let mut stopping = Box::pin(auth.stop());
        let disable = while_pending(stopping.as_mut(), peer.request("Fetch.disable")).await;
        peer.fail(&disable, "Fetch.disable failed").await;
        while_pending(stopping.as_mut(), peer.recover_fetch()).await;
        let disable = while_pending(stopping.as_mut(), peer.request("Network.disable")).await;
        peer.reply(&disable, json!({})).await;
        assert_protocol(&stopping.await.unwrap_err(), "Fetch.disable failed");
        assert!(matches!(auth.status(), ProxyAuthStatus::Failed(_)));
        peer.finish(&session).await;
    })
    .await;
}

#[tokio::test]
async fn fetch_and_network_cleanup_errors_are_both_preserved() {
    bounded(async {
        let (mut peer, session) = Peer::session().await;
        let auth = peer.start(&session, true).await;
        let mut stopping = Box::pin(auth.stop());
        let disable = while_pending(stopping.as_mut(), peer.request("Fetch.disable")).await;
        peer.fail(&disable, "Fetch.disable failed").await;
        while_pending(stopping.as_mut(), peer.recover_fetch()).await;
        let disable = while_pending(stopping.as_mut(), peer.request("Network.disable")).await;
        peer.fail(&disable, "Network.disable failed").await;
        let detach =
            while_pending(stopping.as_mut(), peer.request("Target.detachFromTarget")).await;
        assert_eq!(detach["params"], json!({"sessionId": SESSION}));
        peer.reply(&detach, json!({})).await;
        let Error::Cleanup { source, cleanup } = stopping.await.unwrap_err() else {
            panic!("both Fetch and Network cleanup errors must survive");
        };
        assert_protocol(&source, "Fetch.disable failed");
        assert_protocol(&cleanup, "Network.disable failed");
        assert!(session.is_closed());
        peer.finish(&session).await;
    })
    .await;
}

#[tokio::test]
async fn shared_network_is_disabled_only_by_the_last_owner() {
    bounded(async {
        for auth_stops_first in [true, false] {
            let (mut peer, session) = Peer::session().await;
            let (owner, ()) = tokio::join!(session.enable_network(), peer.ok("Network.enable"));
            let owner = owner.unwrap();
            // An existing Network owner means auth must not enable it again.
            let auth = peer.start(&session, false).await;
            if auth_stops_first {
                let (stopped, ()) = tokio::join!(auth.stop(), peer.ok("Fetch.disable"));
                stopped.unwrap();
                // This root command is a wire barrier: an early Network.disable
                // would be observed instead, with no timing-based absence check.
                let (barrier, ()) = tokio::join!(
                    session.connection().execute_raw("Test.barrier", json!({})),
                    peer.ok("Test.barrier")
                );
                barrier.unwrap();
                let (closed, ()) = tokio::join!(owner.close(), peer.ok("Network.disable"));
                closed.unwrap();
            } else {
                owner.close().await.unwrap();
                let (stopped, ()) = tokio::join!(auth.stop(), async {
                    peer.ok("Fetch.disable").await;
                    peer.ok("Network.disable").await;
                });
                stopped.unwrap();
            }
            peer.finish(&session).await;
        }
    })
    .await;
}

#[tokio::test]
async fn connection_closure_during_network_acquisition_finishes_start() {
    bounded(async {
        let (mut peer, session) = Peer::session().await;
        let mut starting = Box::pin(start(&session));
        while_pending(starting.as_mut(), peer.request("Network.enable")).await;
        peer.0.close(None).await.unwrap();
        assert!(starting.await.is_err());
        assert!(session.is_closed());
        session.connection().close().await;
    })
    .await;
}

#[tokio::test]
async fn connection_closure_after_start_allows_stop_to_finish() {
    bounded(async {
        let (mut peer, session) = Peer::session().await;
        let auth = peer.start(&session, true).await;
        peer.0.close(None).await.unwrap();
        session.closed().await;
        auth.stop().await.unwrap();
        assert_eq!(auth.status(), ProxyAuthStatus::Stopped);
        assert!(session.is_closed());
        session.connection().close().await;
    })
    .await;
}
