//! Bounded local parent/iframe documents for navigation browser regressions.
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const IO_TIMEOUT: Duration = Duration::from_secs(2);

pub struct NavigationServer {
    address: SocketAddr,
    stopped: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl NavigationServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopping = stopped.clone();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                if stopping.load(Ordering::Relaxed) {
                    break;
                }
                // Bound both concurrent workers and retained join handles.
                if requests.len() == 64 {
                    drop(stream);
                    continue;
                }
                requests.push(thread::spawn(move || {
                    // Browser teardown can close a socket during a write.
                    let _ = serve(stream, address.port());
                }));
            }
            for request in requests {
                request.join().unwrap();
            }
        });
        Self {
            address,
            stopped,
            worker: Some(worker),
        }
    }

    pub fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}:{}{path}", self.address.port())
    }
}

impl Drop for NavigationServer {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        // Unblock accept; every request reader has a finite total deadline.
        let _ = TcpStream::connect_timeout(&self.address, IO_TIMEOUT);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(mut stream: TcpStream, port: u16) -> io::Result<()> {
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
    let path = request.split_whitespace().nth(1).unwrap_or("");
    let (status, content) = match path {
        "/" => ("200 OK", format!("<title>Parent</title><iframe src='http://localhost:{port}/child1'></iframe>")),
        "/child1" => ("200 OK", "<title>Before</title><a id='next' href='/child2'>Next</a><iframe src='/nested1'></iframe>".into()),
        "/child2" => ("200 OK", "<title>After</title><iframe src='/nested2'></iframe>".into()),
        "/nested1" | "/nested2" => ("200 OK", "<title>Nested child</title>".into()),
        _ => ("404 Not Found", "Not found".into()),
    };
    let body = format!("<!doctype html><link rel='icon' href='data:,'>{content}");
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
