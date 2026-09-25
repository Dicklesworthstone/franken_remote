//! The real `fr-input-agent --clipboard` child on a private Xvfb: once driven
//! by frd's real `clipboard_process` owner, once by raw protocol frames to
//! reach the child's own final gate. The other X11 application is a separate
//! XCB connection (`X11Clipboard`), observing real selection ownership; no
//! `FrankenRemote` authority exists here (none is needed for a native owner).
#![cfg(all(
    target_os = "linux",
    feature = "linux-input",
    feature = "linux-clipboard"
))]
#![forbid(unsafe_code)]
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use fr_core::{
    clipboard::{
        ClipboardSink, Endpoint, PlatformError, Publication, Stamp,
        process::{self, Reply, Request},
    },
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_native::{clipboard::X11Clipboard, clock};
use fr_wire::clipboard::session::synchronize::{NativeClipboard, NativeText};
use frd::{
    clipboard_process::{RemoteClipboard, factory},
    input_process::ProcessLaunch,
    input_watchdog::host_now,
};
use std::{
    io::{BufRead, BufReader, Read, Write},
    os::{fd::OwnedFd, unix::net::UnixStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const AGENT: &str = env!("CARGO_BIN_EXE_fr-input-agent");

struct Server {
    child: Child,
    display: String,
}
impl Server {
    fn start() -> Self {
        let (reader, writer) = UnixStream::pair().unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let child = Command::new("Xvfb")
            .args([
                "-displayfd",
                "1",
                "-screen",
                "0",
                "320x240x24",
                "-noreset",
                "-nolisten",
                "tcp",
            ])
            .stdout(Stdio::from(OwnedFd::from(writer)))
            .stderr(Stdio::inherit())
            .spawn()
            .expect("real Xvfb required");
        let mut number = String::new();
        BufReader::new(reader.take(16))
            .read_line(&mut number)
            .unwrap();
        Self {
            child,
            display: format!(":{}", number.trim().parse::<u16>().unwrap()),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn stamp(source: Endpoint, sequence: u64) -> Stamp {
    Stamp {
        id: 0xfeed_0000 + u128::from(sequence),
        source,
        sequence,
    }
}
/// Another application copies, through its own X11 connection.
fn copy(app: &mut X11Clipboard, text: &str, sequence: u64) -> Stamp {
    let stamp = stamp(Endpoint::Host, sequence);
    app.prepare(text, stamp).unwrap();
    assert_eq!(app.publish(text, stamp), Publication::SubmittedToOs);
    stamp
}
fn until(limit: Duration, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "bounded X11 exchange timed out");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn the_real_child_reads_a_local_copy_and_publishes_a_peer_item_on_x11() {
    let server = Server::start();
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let launch = ProcessLaunch::new(Path::new(AGENT), &server.display, None, 0xa9e).unwrap();
    let mut owner: RemoteClipboard = factory(launch, cx.clone())().unwrap();
    let mut app = X11Clipboard::open(&server.display, &ProtocolLimits::ABSOLUTE, true).unwrap();

    owner.watch().unwrap();
    let copied = "copied on the host · λ 👋 ü";
    copy(&mut app, copied, 1);
    // XFixes reports the other application's ownership (no content polling).
    until(Duration::from_secs(5), || {
        app.pump().unwrap();
        owner
            .changes()
            .unwrap()
            .latest
            .is_some_and(|c| c.has_selection && c.origin.is_none())
    });
    owner.begin_read().unwrap();
    let mut read = None;
    until(Duration::from_secs(5), || {
        app.pump().unwrap(); // the owning application answers the child
        read = owner.poll_read().unwrap();
        read.is_some()
    });
    let read = read.unwrap();
    assert_eq!((read.text(), read.origin()), (copied, None));

    // A peer item becomes the real X11 CLIPBOARD, read back by the application.
    let item = "from the viewer — ✓ ünïcödé";
    let peer = stamp(Endpoint::Controller, 7);
    owner
        .prepare_for_revision(item, peer, owner.revision())
        .unwrap();
    let deadline = host_now(&cx)
        .unwrap()
        .checked_add(HostDuration::from_micros(1_000_000))
        .unwrap();
    assert_eq!(
        owner.publish_until(item, peer, deadline),
        Publication::SubmittedToOs
    );
    assert_eq!(app.current_origin().unwrap(), None, "ownership moved");
    app.begin_read().unwrap();
    let mut seen = None;
    until(Duration::from_secs(5), || {
        owner.changes().unwrap(); // the child serves the SelectionRequest
        seen = app.poll_read().unwrap();
        seen.is_some()
    });
    assert_eq!(seen.unwrap().as_str(), item);
    NativeClipboard::close(&mut owner);
    drop(server);
}

/// Raw frames to the real child: the test is its frd.
struct Raw {
    child: Child,
    command: UnixStream,
    sequence: u64,
}
impl Raw {
    fn start(display: &str) -> Self {
        let (command, theirs) = UnixStream::pair().unwrap();
        command
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let child = Command::new(AGENT)
            .env_clear()
            .env("DISPLAY", display)
            .arg("--clipboard")
            .arg("--parent-pid")
            .arg(std::process::id().to_string())
            .stdin(Stdio::from(OwnedFd::from(theirs)))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self {
            child,
            command,
            sequence: 0,
        }
    }
    fn send(&mut self, request: Request, payload: &[u8]) {
        self.sequence += 1;
        let frame = process::encode_request(self.sequence, request).unwrap();
        self.command.write_all(&frame).unwrap();
        self.command.write_all(payload).unwrap();
    }
    fn exchange(&mut self, request: Request, payload: &[u8]) -> Reply {
        self.send(request, payload);
        let mut frame = [0; process::FRAME_BYTES];
        self.command.read_exact(&mut frame).unwrap();
        let (echo, reply) = process::decode_reply(&frame, 64).unwrap();
        assert_eq!(echo, self.sequence);
        reply
    }
}

#[test]
fn the_child_rechecks_the_deadline_before_x11_and_refuses_an_oversized_frame() {
    let server = Server::start();
    let mut app = X11Clipboard::open(&server.display, &ProtocolLimits::ABSOLUTE, true).unwrap();
    let mine = copy(&mut app, "the application's own copy", 1);
    let mut raw = Raw::start(&server.display);
    assert_eq!(
        raw.exchange(
            Request::Hello {
                epoch: 42,
                max_item_bytes: 64
            },
            &[]
        ),
        Reply::Ready { epoch: 42 }
    );
    assert!(matches!(
        raw.exchange(Request::Watch, &[]),
        Reply::Watching { .. }
    ));
    let late = "must never become the clipboard";
    let item = stamp(Endpoint::Controller, 3);
    let prepare = Request::Prepare {
        stamp: item,
        revision: None,
        len: u32::try_from(late.len()).unwrap(),
    };
    assert!(matches!(
        raw.exchange(prepare, late.as_bytes()),
        Reply::Prepared { .. }
    ));
    // PLANTED: a deadline already reached when the child's final check runs
    // (a lease revoked or expired while the request was in flight).
    let expired = clock::monotonic_ns().unwrap();
    let reply = raw.exchange(
        Request::Publish {
            stamp: item,
            not_after_ns: expired,
        },
        &[],
    );
    assert!(
        matches!(
            reply,
            Reply::Published {
                publication: Publication::NotSubmitted(PlatformError::Unavailable),
                ..
            }
        ),
        "{reply:?}"
    );
    app.pump().unwrap();
    assert_eq!(
        app.current_origin().unwrap(),
        Some(mine),
        "the late item touched the X11 selection"
    );
    // Control: the same child with a live deadline does take ownership.
    let live = "published in time";
    let item = stamp(Endpoint::Controller, 4);
    assert!(matches!(
        raw.exchange(
            Request::Prepare {
                stamp: item,
                revision: None,
                len: u32::try_from(live.len()).unwrap(),
            },
            live.as_bytes()
        ),
        Reply::Prepared { .. }
    ));
    let reply = raw.exchange(
        Request::Publish {
            stamp: item,
            not_after_ns: clock::monotonic_ns().unwrap() + 2_000_000_000,
        },
        &[],
    );
    assert!(
        matches!(
            reply,
            Reply::Published {
                publication: Publication::SubmittedToOs,
                ..
            }
        ),
        "{reply:?}"
    );
    app.pump().unwrap();
    assert_eq!(app.current_origin().unwrap(), None);
    // An announced item above the Hello bound is refused from its frame: the
    // child exits with its protocol code, reading no payload and replying nothing.
    raw.send(
        Request::Prepare {
            stamp: stamp(Endpoint::Controller, 5),
            revision: None,
            len: 65,
        },
        &[],
    );
    let mut byte = [0; 1];
    assert_eq!(raw.command.read(&mut byte).unwrap(), 0, "no reply");
    assert_eq!(raw.child.wait().unwrap().code(), Some(65));
    drop(server);
}

#[test]
fn a_launch_without_its_parent_or_role_arguments_is_refused_before_x11() {
    for args in [
        vec!["--clipboard"],
        vec!["--clipboard", "--parent-pid", "1"],
        vec!["--parent-pid", "1", "--clipboard"],
    ] {
        let status = Command::new(AGENT)
            .env_clear()
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(64));
    }
}
