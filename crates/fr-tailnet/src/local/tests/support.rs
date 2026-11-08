//! Shared real Unix/HTTP fixture ownership. Request policies remain at callers.
use crate::local::{LocalApi, wall_now};
use asupersync::{net::unix::UnixStream, runtime::RuntimeBuilder};
use serde_json::Value;
use std::{
    future::{Future, poll_fn},
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream as Socket},
    path::Path,
    pin::pin,
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::Poll,
    thread::{self, JoinHandle},
    time::Duration,
};

pub(crate) fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}

pub(crate) async fn both<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
    let (mut a, mut b) = (pin!(a), pin!(b));
    let (mut ar, mut br) = (None, None);
    poll_fn(|cx| {
        if ar.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(cx)
        {
            ar = Some(value);
        }
        if br.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(cx)
        {
            br = Some(value);
        }
        if ar.is_some() && br.is_some() {
            Poll::Ready((ar.take().unwrap(), br.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

pub(crate) fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "fixture OpenSSL: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Serial handlers finish before the next accept. Concurrent handlers return
/// their original thread, retained and joined only after the listener stops.
pub(crate) struct Http {
    pub(crate) client: LocalApi,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Http {
    pub(crate) fn start(
        path: &Path,
        mut handle: impl FnMut(Socket, Arc<AtomicBool>) -> Option<JoinHandle<()>> + Send + 'static,
    ) -> Self {
        let listener = UnixListener::bind(path).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = thread::spawn(move || {
            let mut requests = Vec::new();
            while !stopped.load(Ordering::Acquire) {
                let (socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                if let Some(request) = handle(socket, stopped.clone()) {
                    requests.push(request);
                }
            }
            for request in requests {
                request.join().unwrap();
            }
        });
        let mut client = LocalApi::new(path).unwrap();
        // Test-only UID substitution allows an unprivileged CI runner. The
        // production constructor still requires the installed root authority.
        client.daemon_uid = UnixStream::pair().unwrap().0.peer_cred().unwrap().uid;
        Self {
            client,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for Http {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.thread.take().unwrap().join().unwrap();
    }
}

pub(crate) fn request(
    socket: &mut Socket,
    read_timeout: Duration,
    write_timeout: Option<Duration>,
) -> Option<String> {
    socket.set_read_timeout(Some(read_timeout)).unwrap();
    if let Some(timeout) = write_timeout {
        socket.set_write_timeout(Some(timeout)).unwrap();
    }
    let mut request = Vec::new();
    let mut byte = [0];
    while request.len() < 2048 && !request.ends_with(b"\r\n\r\n") {
        match socket.read(&mut byte) {
            Ok(1) => request.push(byte[0]),
            _ => break,
        }
    }
    if request.is_empty() {
        return None; // A credential refusal can close before sending a query.
    }
    let request = String::from_utf8(request).unwrap();
    assert!(
        request.contains("Host: local-tailscaled.sock\r\n"),
        "exact LocalAPI Host required"
    );
    Some(request)
}

pub(crate) struct Server {
    pub(crate) client: LocalApi,
    pub(crate) calls: Arc<AtomicUsize>,
    _http: Http,
}
impl Server {
    pub(crate) fn new(replies: Vec<Vec<u8>>, pause: Duration) -> Self {
        Self::with_reply_hook(replies, move |_| thread::sleep(pause))
    }
    pub(crate) fn with_reply_hook(
        replies: Vec<Vec<u8>>,
        before_reply: impl FnMut(usize) + Send + 'static,
    ) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-localapi-{}-{}-{}.sock",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst),
            wall_now().unwrap()
        ));
        Self::start(
            &path,
            replies,
            before_reply,
            |request| {
                assert!(
                    request.starts_with("GET /localapi/v0/status?peers=true ")
                        || request.starts_with("GET /localapi/v0/status?peers=false ")
                        || request.starts_with("GET /localapi/v0/whois?addr=100.64.0.2%3A30001 ")
                );
            },
            true,
        )
    }
    pub(crate) fn status_only(
        path: &Path,
        replies: Vec<Vec<u8>>,
        pause: Duration,
        check_write: bool,
    ) -> Self {
        Self::start(
            path,
            replies,
            move |_| thread::sleep(pause),
            |request| {
                assert!(request.starts_with("GET /localapi/v0/status?peers=true "));
            },
            check_write,
        )
    }
    fn start(
        path: &Path,
        replies: Vec<Vec<u8>>,
        mut before_reply: impl FnMut(usize) + Send + 'static,
        check: fn(&str),
        check_write: bool,
    ) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let mut index = 0;
        let http = Http::start(path, move |mut socket, _| {
            if let Some(request) = request(&mut socket, Duration::from_millis(300), None) {
                check(&request);
                counted.fetch_add(1, Ordering::SeqCst);
                before_reply(index);
                let reply = &replies[index.min(replies.len() - 1)];
                index += 1;
                // Metadata tests qualify cancellation/oversize write failures;
                // dial fixtures deliberately ignore them, as before.
                if let Err(error) = socket.write_all(reply)
                    && check_write
                {
                    assert!(matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ));
                }
            }
            None
        });
        Self {
            client: http.client.clone(),
            calls,
            _http: http,
        }
    }
}

pub(crate) fn response(value: &Value, chunked: bool) -> Vec<u8> {
    let data = serde_json::to_vec(value).unwrap();
    if chunked {
        let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        for part in data.chunks(17) {
            write!(out, "{:x}\r\n", part.len()).unwrap();
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
        out
    } else {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",data.len()).into_bytes();
        out.extend_from_slice(&data);
        out
    }
}
