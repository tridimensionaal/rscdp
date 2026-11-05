//! Bounded, loopback-only proxy for authentication browser regressions.
use rscdp::{Error, Result, proxy::Proxy};
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Arc, Condvar, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};
use tokio::sync::Notify;

const PATHS: [&str; 3] = ["/", "/hold", "/healthy"];
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct State {
    stopped: bool,
    close_hold: bool,
    authenticated: [usize; 3],
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    wake: Condvar,
    arrived: Notify,
}

pub struct AuthProxy {
    address: SocketAddr,
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl AuthProxy {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let shared = Arc::new(Shared::default());
        let serving = shared.clone();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                if serving.state.lock().unwrap().stopped {
                    break;
                }
                // Bound thread and retained handle counts, including browser
                // background connections. No external destinations are contacted.
                if requests.len() == 64 {
                    drop(stream);
                    continue;
                }
                let shared = serving.clone();
                requests.push(thread::spawn(move || {
                    // Browser cancellation can close a socket during any write.
                    let _ = serve(stream, &shared);
                }));
            }
            for request in requests {
                request.join().unwrap();
            }
        });
        Self {
            address,
            shared,
            worker: Some(worker),
        }
    }

    pub fn proxy(&self) -> Proxy {
        format!("http://test:secret@{}", self.address)
            .parse()
            .unwrap()
    }

    pub fn url(&self, path: &str) -> String {
        fixture_url(path)
    }

    pub fn authenticated_requests(&self, path: &str) -> usize {
        let index = PATHS
            .iter()
            .position(|candidate| *candidate == path)
            .unwrap();
        self.shared.state.lock().unwrap().authenticated[index]
    }

    pub async fn wait_for_authenticated_request(&self, path: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let arrived = self.shared.arrived.notified();
                if self.authenticated_requests(path) > 0 {
                    return;
                }
                arrived.await;
            }
        })
        .await
        .map_err(|_| {
            Error::Timeout(format!(
                "proxy received no authenticated request for {}",
                self.url(path)
            ))
        })
    }

    pub fn close_without_headers(&self, path: &str) {
        assert_eq!(path, "/hold");
        self.shared.state.lock().unwrap().close_hold = true;
        self.shared.wake.notify_all();
    }
}

impl Drop for AuthProxy {
    fn drop(&mut self) {
        self.shared.state.lock().unwrap().stopped = true;
        self.shared.wake.notify_all();
        // Wake blocking accept; request readers have finite total deadlines.
        let _ = TcpStream::connect_timeout(&self.address, IO_TIMEOUT);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn fixture_url(path: &str) -> String {
    format!("http://auth-proxy.test{path}")
}

fn serve(mut stream: TcpStream, shared: &Shared) -> io::Result<()> {
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut request = Vec::new();
    let mut bytes = [0; 1024];
    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(());
        }
        stream.set_read_timeout(Some(remaining))?;
        let count = stream.read(&mut bytes)?;
        if count == 0 || request.len() + count > 8192 {
            return Ok(());
        }
        request.extend_from_slice(&bytes[..count]);
    }
    let request = String::from_utf8_lossy(&request);
    let url = request.split_whitespace().nth(1).unwrap_or("");
    let Some(index) = PATHS.iter().position(|path| url == fixture_url(path)) else {
        return stream.write_all(
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
    };
    let authenticated =
        request
            .lines()
            .filter_map(|line| line.split_once(':'))
            .any(|(name, value)| {
                name.eq_ignore_ascii_case("Proxy-Authorization")
                    && value.trim() == "Basic dGVzdDpzZWNyZXQ="
            });
    if !authenticated {
        return stream.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"fixture\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
    let mut state = shared.state.lock().unwrap();
    state.authenticated[index] += 1;
    shared.arrived.notify_one();
    if PATHS[index] == "/hold" {
        // A finite fallback prevents a broken test from retaining a worker.
        // Normal shutdown and the EOF control wake this immediately.
        let _ = shared
            .wake
            .wait_timeout_while(state, Duration::from_secs(30), |state| {
                !state.stopped && !state.close_hold
            })
            .unwrap();
        return Ok(());
    }
    drop(state);
    let body = "<!doctype html><title>Authenticated proxy fixture</title>";
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
