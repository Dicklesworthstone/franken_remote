//! A busy pipe, planted with a one-instance server: PowerShell's
//! `NamedPipeServerStream`, since this crate makes no Win32 calls itself.
use super::open;
use crate::Error;
use asupersync::runtime::RuntimeBuilder;
use fr_winsys::pipe::PipeClient;
use std::{
    future::Future,
    path::PathBuf,
    pin::pin,
    process::{Child, Command},
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

/// Holds its only instance for `hold` after the first client connects, then
/// offers a second instance and keeps it until the second client arrives.
struct Server {
    path: PathBuf,
    child: Child,
}

impl Server {
    fn start(hold: Duration) -> Self {
        let name = format!(
            "fr-tailnet-busy-{}-{}",
            std::process::id(),
            hold.as_millis()
        );
        let script = format!(
            "$one = New-Object System.IO.Pipes.NamedPipeServerStream('{name}', 'InOut', 1); \
             $one.WaitForConnection(); Start-Sleep -Milliseconds {}; $one.Dispose(); \
             $two = New-Object System.IO.Pipes.NamedPipeServerStream('{name}', 'InOut', 1); \
             $two.WaitForConnection(); Start-Sleep -Milliseconds 500; $two.Dispose()",
            hold.as_millis()
        );
        let child = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &script])
            .spawn()
            .unwrap();
        Self {
            path: PathBuf::from(format!(r"\\.\pipe\{name}")),
            child,
        }
    }

    /// The first client: it takes the only instance once the server is up.
    fn occupy(&self) -> PipeClient {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(client) = PipeClient::open(&self.path) {
                return client;
            }
            assert!(Instant::now() < deadline, "the pipe server never came up");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn an_open_waits_out_a_busy_pipe_inside_the_runtime() {
    let hold = Duration::from_millis(300);
    let server = Server::start(hold);
    let first = server.occupy();
    let began = Instant::now();
    let runtime = RuntimeBuilder::new()
        .worker_threads(1)
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    let second = runtime.block_on(open(&server.path));
    let waited = began.elapsed();
    assert!(second.is_ok(), "{:?}", second.err());
    // The server offers its second instance only after `hold`.
    assert!(waited >= hold / 2, "opened after {waited:?}");
    drop(first);
}

#[test]
fn without_a_runtime_context_a_busy_pipe_is_unavailable_at_once() {
    let server = Server::start(Duration::from_millis(3_000));
    let first = server.occupy();
    let began = Instant::now();
    let mut opening = pin!(open(&server.path));
    let outcome = opening
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
        .map(|opened| opened.map(drop));
    assert!(
        matches!(outcome, Poll::Ready(Err(Error::LocalApiUnavailable))),
        "{outcome:?}"
    );
    assert!(began.elapsed() < Duration::from_secs(1));
    drop(first);
}
