//! The real macOS `fr-input-agent` over its private protocol, launched the way
//! frd's `input_process` launches it: cleared environment, `--parent-pid`, the
//! command stream on stdin and the signal socket on stdout.
//!
//! Nothing here posts an input event. The probe, launch and refusal tests
//! need only the Accessibility grant. The lease test shows the control
//! indicator on the real display for a moment, so it is `#[ignore]`d; run it
//! with `--ignored`. Injected input is qualified live, with the owner's OK.
#![cfg(all(target_os = "macos", feature = "macos-input"))]
use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey},
    input_submission::{
        Capabilities, Capability, Operation, PlatformError,
        process::{self, Reply, Request, Signal},
    },
};
use fr_native::macos_input;
use std::{
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::{
            net::{UnixDatagram, UnixStream},
            process::CommandExt,
        },
    },
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const AGENT: &str = env!("CARGO_BIN_EXE_fr-input-agent");

struct Agent {
    child: Child,
    command: UnixStream,
    signals: UnixDatagram,
}

fn launch(args: &[&str]) -> Agent {
    let (command, child_command) = UnixStream::pair().unwrap();
    let (signals, child_signals) = UnixDatagram::pair().unwrap();
    signals.set_nonblocking(true).unwrap();
    let child = Command::new(AGENT)
        .env_clear()
        .args(args)
        .stdin(Stdio::from(OwnedFd::from(child_command)))
        .stdout(Stdio::from(OwnedFd::from(child_signals)))
        .stderr(Stdio::inherit())
        .process_group(0)
        .spawn()
        .unwrap();
    command
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    Agent {
        child,
        command,
        signals,
    }
}

fn parent() -> String {
    std::process::id().to_string()
}

impl Agent {
    fn started() -> Self {
        launch(&["--parent-pid", &parent()])
    }
    fn send(&mut self, sequence: u64, request: Request) {
        let frame = process::encode_request(sequence, request).unwrap();
        self.command.write_all(&frame).unwrap();
    }
    fn reply(&mut self, sequence: u64) -> Reply {
        let mut frame = [0; process::FRAME_BYTES];
        self.command.read_exact(&mut frame).unwrap();
        let (echo, reply) = process::decode_reply(&frame).unwrap();
        assert_eq!(echo, sequence);
        reply
    }
    fn exchange(&mut self, sequence: u64, request: Request) -> Reply {
        self.send(sequence, request);
        self.reply(sequence)
    }
    fn exit(&mut self) -> ExitStatus {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < until, "the agent did not exit");
            thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn implemented() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
        .with(Capability::PixelScroll)
}

fn require_access() {
    assert!(
        macos_input::access(),
        "grant Accessibility to the terminal running these tests"
    );
}

#[test]
fn probe_reports_what_a_lease_executor_would_advertise_and_exits() {
    require_access();
    let mut agent = Agent::started();
    let reply = agent.exchange(1, Request::Probe { epoch: 7 });
    assert!(matches!(
        reply,
        Reply::Ready {
            epoch: 7,
            capabilities,
            repeat_requires_pair: false,
            line_scroll_requires_pairs: false,
        } if capabilities == implemented()
    ));
    assert_eq!(agent.exit().code(), Some(0));
}

#[test]
fn a_launch_without_its_exact_parent_is_refused_before_any_native_call() {
    for args in [
        &[][..],
        &["--parent-pid", "1"][..],
        &["--parent-pid", "not-a-pid"][..],
        &["--parent-pid", &parent(), "extra"][..],
        // The X11 clipboard owner role has no macOS backend.
        &["--clipboard", "--parent-pid", &parent()][..],
    ] {
        let mut agent = launch(args);
        assert_eq!(agent.exit().code(), Some(64), "{args:?}");
    }
}

#[test]
fn a_grant_naming_no_current_display_is_refused() {
    require_access();
    let displays = macos_input::display_bounds().unwrap();
    // One pixel off every display's exact rectangle.
    let (_, real) = *displays.first().expect("an active display");
    let shifted = InputBounds::new(
        DesktopPoint {
            x: real.origin().x + 1,
            y: real.origin().y,
        },
        real.width(),
        real.height(),
    )
    .unwrap();
    assert!(displays.iter().all(|(_, b)| *b != shifted));
    let mut agent = Agent::started();
    let reply = agent.exchange(
        1,
        Request::Hello {
            epoch: 9,
            bounds: shifted,
            required: implemented(),
        },
    );
    assert!(matches!(
        reply,
        Reply::Refused(PlatformError::GeometryChanged)
    ));
    assert_eq!(agent.exit().code(), Some(66));
}

#[test]
fn a_request_other_than_hello_or_probe_first_is_a_protocol_violation() {
    let mut agent = Agent::started();
    agent.send(1, Request::Stop);
    assert_eq!(agent.exit().code(), Some(65));
}

/// A real lease on the main display, driven only with operations that post
/// nothing: refusals, a fence and release-only cleanup.
#[test]
#[ignore = "shows the control indicator on the real display (no input is posted)"]
fn a_lease_refuses_without_posting_then_fences_and_stops() {
    require_access();
    let (_, bounds) = macos_input::display_bounds().unwrap()[0];
    let mut agent = Agent::started();
    let reply = agent.exchange(
        1,
        Request::Hello {
            epoch: 11,
            bounds,
            required: implemented(),
        },
    );
    assert!(matches!(
        reply,
        Reply::Ready {
            epoch: 11,
            capabilities,
            repeat_requires_pair: false,
            line_scroll_requires_pairs: false,
        } if capabilities == implemented()
    ));
    let outside = DesktopPoint {
        x: bounds.origin().x - 1,
        y: bounds.origin().y,
    };
    assert!(matches!(
        agent.exchange(2, Request::Prepare(Operation::Absolute(outside))),
        Reply::PrepareFailed(PlatformError::GeometryChanged)
    ));
    let a = PhysicalKey::new(0x04).unwrap();
    let release = Operation::Key {
        key: a,
        transition: KeyTransition::Release,
    };
    assert!(matches!(
        agent.exchange(3, Request::Prepare(release)),
        Reply::PrepareFailed(PlatformError::Unsupported)
    ));
    let repeat = Operation::Key {
        key: a,
        transition: KeyTransition::Repeat,
    };
    assert!(matches!(
        agent.exchange(4, Request::Prepare(repeat)),
        Reply::PrepareFailed(PlatformError::Unsupported)
    ));
    // A deadline already in the past: Expired, and nothing is posted.
    let inside = Operation::Absolute(bounds.origin());
    assert!(matches!(
        agent.exchange(5, Request::Prepare(inside)),
        Reply::Prepared
    ));
    assert!(matches!(
        agent.exchange(
            6,
            Request::Submit {
                operation: inside,
                not_after_ns: 1,
            }
        ),
        Reply::Expired
    ));
    // After a fence no positive operation is even prepared.
    agent
        .signals
        .send(&process::encode_signal(Signal::Fence))
        .unwrap();
    assert!(matches!(
        agent.exchange(7, Request::Prepare(inside)),
        Reply::Fenced
    ));
    assert!(matches!(
        agent.exchange(8, Request::Cleanup),
        Reply::Cleaned(true)
    ));
    assert!(matches!(agent.exchange(9, Request::Stop), Reply::Stopped));
    assert_eq!(agent.exit().code(), Some(0));
    // The indicator never reported a local stop or a failure.
    let mut buffer = [0; process::SIGNAL_BYTES];
    assert!(agent.signals.recv(&mut buffer).is_err());
}

/// Frames out of sequence end the lease: the executor never guesses.
#[test]
#[ignore = "shows the control indicator on the real display (no input is posted)"]
fn a_lease_ends_on_an_out_of_sequence_request() {
    require_access();
    let (_, bounds) = macos_input::display_bounds().unwrap()[0];
    let mut agent = Agent::started();
    let reply = agent.exchange(
        1,
        Request::Hello {
            epoch: 13,
            bounds,
            required: implemented(),
        },
    );
    assert!(matches!(reply, Reply::Ready { epoch: 13, .. }));
    agent.send(3, Request::Cleanup);
    assert_eq!(agent.exit().code(), Some(65));
}
