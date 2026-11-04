use super::*;
use crate::{Connection, cdp::target::TargetId};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

struct Peer {
    handler: Handler,
    commands: mpsc::UnboundedReceiver<Value>,
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
        let (commands, received) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(Message::Text(text))) = socket.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let result = if request["method"] == "Target.attachToTarget" {
                    json!({"sessionId":"session"})
                } else {
                    commands.send(request.clone()).unwrap();
                    json!({})
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
        let session = connection.attach(TargetId("tab".into())).await.unwrap();
        let (status, _) = watch::channel(ProxyAuthStatus::Pending);
        let handler = Handler {
            session,
            proxy: "http://user:password@127.0.0.1:8080".parse().unwrap(),
            status,
            attempted: HashSet::new(),
            challenges: VecDeque::new(),
            requests: VecDeque::new(),
            network_requests: HashMap::new(),
            cold: None,
            probe: None,
            gate_open: false,
            verified: false,
            rejected: false,
        };
        Self {
            handler,
            commands: received,
            task,
        }
    }

    async fn event(&mut self, id: &str, network: Option<&str>, kind: AuthEventKind) {
        self.handler
            .event(AuthEvent {
                id: id.into(),
                network_id: network.map(str::to_owned),
                kind,
            })
            .await
            .unwrap();
    }

    async fn request(&mut self, id: &str, network: &str) {
        self.event(id, Some(network), AuthEventKind::Request).await;
    }

    async fn challenge(&mut self, id: &str) {
        self.event(
            id,
            None,
            AuthEventKind::Challenge {
                is_proxy: true,
                origin: Some("http://127.0.0.1:8080".into()),
            },
        )
        .await;
    }

    async fn failed(&mut self, network: &str) {
        self.event(network, None, AuthEventKind::NetworkFailure)
            .await;
    }

    fn take_commands(&mut self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        while let Ok(command) = self.commands.try_recv() {
            out.push((
                command["method"].as_str().unwrap().into(),
                command["params"]["requestId"].as_str().unwrap().into(),
            ));
        }
        out
    }
}

#[tokio::test]
async fn network_failure_retires_cold_and_probe_once() {
    let mut peer = Peer::new().await;
    peer.request("fetch-1", "net-1").await;
    peer.challenge("fetch-1").await;
    peer.request("fetch-2", "net-2").await;
    peer.take_commands();
    peer.failed("net-1").await;
    peer.failed("net-1").await;
    assert_eq!(
        peer.take_commands(),
        [("Fetch.continueRequest".into(), "fetch-2".into())]
    );
    assert_eq!(peer.handler.cold.as_deref(), Some("fetch-2"));
    assert!(peer.handler.probe.is_none());
    assert!(!peer.handler.attempted.contains("fetch-1"));
    assert_eq!(*peer.handler.status.borrow(), ProxyAuthStatus::Pending);
}

#[tokio::test]
async fn network_failure_removes_canceled_queued_request() {
    let mut peer = Peer::new().await;
    peer.request("one", "n1").await;
    peer.request("two", "n2").await;
    peer.request("three", "n3").await;
    peer.take_commands();
    peer.failed("n2").await;
    assert!(peer.take_commands().is_empty());
    peer.failed("n1").await;
    assert_eq!(
        peer.take_commands(),
        [("Fetch.continueRequest".into(), "three".into())]
    );
}

#[tokio::test]
async fn fetch_error_then_network_failure_is_idempotent() {
    let mut peer = Peer::new().await;
    peer.request("one", "n1").await;
    peer.challenge("one").await;
    peer.request("two", "n2").await;
    peer.take_commands();
    peer.event(
        "one",
        Some("n1"),
        AuthEventKind::Response {
            rejected: false,
            failed: true,
        },
    )
    .await;
    assert_eq!(
        peer.take_commands(),
        [
            ("Fetch.continueRequest".into(), "one".into()),
            ("Fetch.continueRequest".into(), "two".into())
        ]
    );
    peer.failed("n1").await;
    assert!(peer.take_commands().is_empty());
    assert_eq!(peer.handler.cold.as_deref(), Some("two"));
}

#[tokio::test]
async fn redirect_correlation_tracks_current_fetch_request() {
    let mut peer = Peer::new().await;
    peer.request("old", "shared").await;
    peer.event(
        "old",
        Some("shared"),
        AuthEventKind::Response {
            rejected: false,
            failed: false,
        },
    )
    .await;
    peer.request("new", "shared").await;
    peer.challenge("new").await;
    // Late predecessor cleanup must not erase the successor's Network mapping.
    peer.event(
        "old",
        Some("shared"),
        AuthEventKind::Response {
            rejected: false,
            failed: true,
        },
    )
    .await;
    peer.take_commands();
    peer.failed("shared").await;
    assert!(peer.handler.probe.is_none());
    assert!(!peer.handler.attempted.contains("new"));
    assert!(peer.take_commands().is_empty());
    assert!(peer.handler.network_requests.is_empty());
}

#[tokio::test]
async fn unknown_terminal_identity_does_not_clear_another_request() {
    let mut peer = Peer::new().await;
    peer.request("fetch", "network").await;
    peer.challenge("fetch").await;
    peer.take_commands();
    // Same spelling in a different ID namespace is still unrelated.
    peer.failed("fetch").await;
    peer.failed("unknown").await;
    assert_eq!(peer.handler.probe.as_deref(), Some("fetch"));
    assert_eq!(*peer.handler.status.borrow(), ProxyAuthStatus::Pending);
    assert!(peer.take_commands().is_empty());
}

#[tokio::test]
async fn terminal_failure_preserves_rejection_and_origin_checks() {
    let mut peer = Peer::new().await;
    peer.request("one", "n1").await;
    peer.challenge("one").await;
    peer.challenge("one").await;
    assert_eq!(*peer.handler.status.borrow(), ProxyAuthStatus::Rejected);
    peer.failed("n1").await;
    assert_eq!(*peer.handler.status.borrow(), ProxyAuthStatus::Rejected);
    peer.take_commands();
    peer.request("two", "n2").await;
    assert_eq!(
        peer.take_commands(),
        [("Fetch.failRequest".into(), "two".into())]
    );

    let mut peer = Peer::new().await;
    peer.request("one", "n1").await;
    peer.take_commands();
    let result = peer
        .handler
        .event(AuthEvent {
            id: "one".into(),
            network_id: None,
            kind: AuthEventKind::Challenge {
                is_proxy: true,
                origin: Some("http://attacker.invalid:8080".into()),
            },
        })
        .await;
    assert!(matches!(result, Err(Error::ProxyAuthentication(_))));
    let command = peer.commands.try_recv().unwrap();
    assert_eq!(
        command["params"]["authChallengeResponse"],
        json!({"response":"CancelAuth"})
    );
}

#[tokio::test]
async fn missing_active_correlation_and_tracking_overflow_fail_explicitly() {
    let mut peer = Peer::new().await;
    let missing = peer
        .handler
        .event(AuthEvent {
            id: "missing".into(),
            network_id: None,
            kind: AuthEventKind::Request,
        })
        .await;
    assert!(matches!(missing, Err(Error::ProxyAuthentication(_))));
    assert!(peer.take_commands().is_empty());
    // An open gate allows outstanding requests; correlation must remain bounded.
    peer.handler.gate_open = true;
    for i in 0..4096 {
        peer.request(&format!("f{i}"), &format!("n{i}")).await;
        peer.take_commands();
    }
    let overflow = peer
        .handler
        .event(AuthEvent {
            id: "extra".into(),
            network_id: Some("extra-network".into()),
            kind: AuthEventKind::Request,
        })
        .await;
    assert!(matches!(overflow, Err(Error::ProxyAuthentication(_))));
    assert!(peer.take_commands().is_empty());

    let mut peer = Peer::new().await;
    // The map plus cold/probe/attempted copies all count toward the text cap.
    let id = "x".repeat(1024 * 1024 + 1);
    peer.request(&id, "n").await;
    peer.take_commands();
    let overflow = peer
        .handler
        .event(AuthEvent {
            id,
            network_id: None,
            kind: AuthEventKind::Challenge {
                is_proxy: true,
                origin: Some("http://127.0.0.1:8080".into()),
            },
        })
        .await;
    assert!(matches!(overflow, Err(Error::ProxyAuthentication(_))));
    assert!(peer.take_commands().is_empty());
}
