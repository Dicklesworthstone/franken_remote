//! Against a real pipe server in this process (Windows only).
use super::PipeClient;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_PIPE_CONNECTED, HANDLE};
use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::core::HSTRING;

/// Wakes the test thread and counts the wakes.
struct Unpark {
    thread: Thread,
    wakes: AtomicUsize,
}
impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
        self.thread.unpark();
    }
}

/// Polls until ready, parking between polls; `None` past `limit`. A task
/// that is never woken parks until `limit`, so tests that finish well
/// inside it were woken by the pipe's completion callback.
fn block_on<T>(
    unpark: &Arc<Unpark>,
    limit: Duration,
    mut poll: impl FnMut(&Context<'_>) -> Poll<T>,
) -> Option<T> {
    let waker = Waker::from(unpark.clone());
    let cx = Context::from_waker(&waker);
    let end = Instant::now() + limit;
    loop {
        if let Poll::Ready(value) = poll(&cx) {
            return Some(value);
        }
        thread::park_timeout(end.checked_duration_since(Instant::now())?);
    }
}

fn unpark() -> Arc<Unpark> {
    Arc::new(Unpark {
        thread: thread::current(),
        wakes: AtomicUsize::new(0),
    })
}

fn name(tag: &str) -> String {
    format!(r"\\.\pipe\fr-winsys-test-{}-{tag}", std::process::id())
}

/// A one-instance, blocking byte-mode server end, created before any client
/// opens it.
fn server(name: &str) -> File {
    // SAFETY: a new local pipe instance with default security.
    let handle = unsafe {
        CreateNamedPipeW(
            &HSTRING::from(name),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            4096,
            4096,
            0,
            None,
        )
    };
    assert!(!handle.is_invalid(), "CreateNamedPipeW");
    // SAFETY: the new pipe handle, owned only by the returned `File`.
    unsafe { File::from_raw_handle(handle.0) }
}

fn accept(server: &File) {
    // SAFETY: a live blocking pipe handle; no `OVERLAPPED`.
    match unsafe { ConnectNamedPipe(HANDLE(server.as_raw_handle()), None) } {
        Ok(()) => {}
        Err(error) if error.code() == ERROR_PIPE_CONNECTED.to_hresult() => {}
        Err(error) => panic!("ConnectNamedPipe: {error}"),
    }
}

// A runtime may move a task holding a client between threads.
const _: fn() = || {
    fn send<T: Send>() {}
    send::<PipeClient>();
};

#[test]
fn only_the_local_pipe_namespace_is_opened() {
    for path in [
        r"C:\Windows\win.ini",
        r"\\.\pipe\",
        r"\\server\pipe\x",
        "relative",
    ] {
        assert_eq!(
            PipeClient::open(Path::new(path)).err().map(|e| e.kind()),
            Some(ErrorKind::InvalidInput),
            "{path}"
        );
    }
    assert_eq!(
        PipeClient::open(Path::new(&name("absent")))
            .err()
            .map(|e| e.kind()),
        Some(ErrorKind::NotFound)
    );
}

#[test]
fn a_request_and_its_late_reply_cross_the_pipe() {
    let name = name("exchange");
    let server = server(&name);
    let request = b"GET /localapi/v0/status HTTP/1.1\r\n\r\n".to_vec();
    // Longer than one kernel read (16 KiB), so reads continue across
    // completions and hand out leftover bytes.
    let reply: Vec<u8> = (0..40_000_u32).map(|i| (i % 251) as u8).collect();
    let expected = (request.len(), reply.clone());
    let worker = thread::spawn(move || {
        let mut server = server;
        accept(&server);
        let mut got = vec![0; expected.0];
        server.read_exact(&mut got).expect("request");
        // The client is parked in a pending read by now.
        thread::sleep(Duration::from_millis(200));
        server.write_all(&expected.1[..10_000]).expect("reply");
        thread::sleep(Duration::from_millis(50));
        server.write_all(&expected.1[10_000..]).expect("reply");
        got
    });

    let mut client = PipeClient::open(Path::new(&name)).expect("open");
    assert_eq!(client.server_process_id(), std::process::id());
    let unpark = unpark();
    let limit = Duration::from_secs(5);
    let started = Instant::now();
    let mut at = 0;
    while at < request.len() {
        at += block_on(&unpark, limit, |cx| client.poll_write(cx, &request[at..]))
            .expect("write in time")
            .expect("write");
    }
    block_on(&unpark, limit, |cx| client.poll_flush(cx))
        .expect("flush in time")
        .expect("flush");
    let mut received = Vec::new();
    let mut out = [0_u8; 4096];
    loop {
        let read = block_on(&unpark, limit, |cx| client.poll_read(cx, &mut out))
            .expect("read in time")
            .expect("read");
        if read == 0 {
            break;
        }
        received.extend_from_slice(&out[..read]);
    }
    let elapsed = started.elapsed();
    assert_eq!(worker.join().expect("server"), request);
    assert_eq!(received, reply);
    // Woken by completions, not by the park timeout.
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert!(unpark.wakes.load(Ordering::SeqCst) > 0);
}

#[test]
fn dropping_a_client_with_a_pending_read_cancels_it() {
    let name = name("cancel");
    let server = server(&name);
    let (release, released) = mpsc::channel::<()>();
    let (connected, accepted) = mpsc::channel::<()>();
    let worker = thread::spawn(move || {
        let mut server = server;
        accept(&server);
        connected.send(()).expect("connected");
        // Never replies until the client is gone.
        released.recv().expect("release");
        server.write_all(b"late").err().map(|e| e.kind())
    });

    let mut client = PipeClient::open(Path::new(&name)).expect("open");
    let unpark = unpark();
    let waker = Waker::from(unpark.clone());
    let cx = Context::from_waker(&waker);
    let mut out = [0_u8; 64];
    assert!(client.poll_read(&cx, &mut out).is_pending());
    // A client gone before the server thread reached ConnectNamedPipe would
    // fail the accept (the pipe is closing), not test cancellation.
    accepted.recv().expect("accepted");
    let started = Instant::now();
    drop(client);
    let elapsed = started.elapsed();
    release.send(()).expect("release");
    // The cancelled read was drained, the handle closed: the server's write
    // finds no client.
    assert!(worker.join().expect("server").is_some());
    assert!(elapsed < Duration::from_secs(1), "{elapsed:?}");
}
