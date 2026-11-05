//! A loopback HTTP fixture only for response-body browser tests.
use std::{
    io::{self, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Default)]
struct State {
    stopped: AtomicBool,
    same_requests: AtomicUsize,
    released: Mutex<bool>,
    wake: Condvar,
}

pub struct ResponseServer {
    address: SocketAddr,
    state: Arc<State>,
    worker: Option<JoinHandle<()>>,
}

impl ResponseServer {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = Arc::new(State::default());
        let serving = state.clone();
        let worker = thread::spawn(move || {
            let mut requests = Vec::new();
            while !serving.stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let state = serving.clone();
                        requests.push(thread::spawn(move || {
                            // Client cancellation may close a connection mid-write.
                            let _ = serve(stream, &state);
                        }));
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("accept response fixture request: {error}"),
                }
            }
            for request in requests {
                request.join().unwrap();
            }
        });
        Self {
            address,
            state,
            worker: Some(worker),
        }
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.address)
    }

    pub fn release_body(&self) {
        *self.state.released.lock().unwrap() = true;
        self.state.wake.notify_all();
    }
}

impl Drop for ResponseServer {
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Relaxed);
        self.release_body();
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(mut stream: TcpStream, state: &State) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    let mut bytes = [0; 1024];
    while !request.windows(4).any(|part| part == b"\r\n\r\n") {
        let count = stream.read(&mut bytes)?;
        if count == 0 || request.len() + count > 8192 {
            return Ok(());
        }
        request.extend_from_slice(&bytes[..count]);
    }
    let request = String::from_utf8_lossy(&request);
    let path = request.split_whitespace().nth(1).unwrap_or("");
    let mut status = "200 OK";
    let mut mime = "text/plain; charset=utf-8";
    let mut extra = "";
    let mut held = false;
    let body: &[u8] = match path {
        "/" => {
            mime = "text/html; charset=utf-8";
            b"<!doctype html><link rel='icon' href='data:,'><title>Response fixture</title>"
        }
        "/text" => "hello, café".as_bytes(),
        "/json" => {
            mime = "application/json";
            br#"{"answer":42}"#
        }
        "/binary" => {
            mime = "application/octet-stream";
            b"\x00\xff\x80\x01"
        }
        "/redirect" => {
            status = "302 Found";
            extra = "Location: /text\r\n";
            b""
        }
        "/delayed" | "/truncated" => {
            held = true;
            b"delayed body"
        }
        "/same" if state.same_requests.fetch_add(1, Ordering::Relaxed) == 0 => {
            held = true;
            b"first"
        }
        "/same" => b"second",
        _ => {
            status = "404 Not Found";
            b"useful missing body"
        }
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: {mime}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n{extra}\r\n",
        body.len()
    )?;
    if held {
        stream.write_all(&body[..1])?;
        stream.flush()?;
        let (released, _) = state
            .wake
            .wait_timeout_while(
                state.released.lock().unwrap(),
                Duration::from_secs(10),
                |released| !*released,
            )
            .unwrap();
        if !*released || path == "/truncated" {
            // EOF before Content-Length produces loadingFailed after headers.
            return Ok(());
        }
        stream.write_all(&body[1..])?;
    } else {
        stream.write_all(body)?;
    }
    Ok(())
}
