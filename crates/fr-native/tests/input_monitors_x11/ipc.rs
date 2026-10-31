//! Actual private Prepare/Submit IPC to the shipping native input-agent image.
//! No runtime, fake executor or direct native call stands in for that child.
use super::*;
use fr_core::input_submission::process::{self, Reply, Request};
use std::{
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::net::{UnixDatagram, UnixStream},
    },
    process::{Child, Command, Stdio},
    time::Duration,
};

struct Agent {
    child: Child,
    command: UnixStream,
    _signals: UnixDatagram,
    sequence: u64,
}
impl Agent {
    fn start(screen: &Screen) -> Self {
        // Ordinary Cargo test supplies its just-built image. The isolated
        // native test harness supplies the same actual build explicitly.
        let image = std::env::var_os("FR_TEST_INPUT_AGENT")
            .or_else(|| option_env!("CARGO_BIN_EXE_fr-input-agent").map(Into::into))
            .expect("build fr-input-agent for the selected-monitor native regression");
        let (command, child_command) = UnixStream::pair().unwrap();
        let (signals, child_signals) = UnixDatagram::pair().unwrap();
        command
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        command
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let child = Command::new(image)
            .env_clear()
            .env("DISPLAY", &screen.name)
            .args(["--parent-pid", &std::process::id().to_string()])
            .stdin(Stdio::from(OwnedFd::from(child_command)))
            .stdout(Stdio::from(OwnedFd::from(child_signals)))
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut this = Self {
            child,
            command,
            _signals: signals,
            sequence: 0,
        };
        assert!(matches!(
            this.exchange(Request::Hello {
                epoch: 1,
                bounds: bounds(),
                required: Capabilities::default()
            }),
            Reply::Ready { epoch: 1, .. }
        ));
        this
    }
    fn exchange(&mut self, request: Request) -> Reply {
        self.sequence += 1;
        self.command
            .write_all(&process::encode_request(self.sequence, request).unwrap())
            .unwrap();
        let mut bytes = [0; process::FRAME_BYTES];
        self.command.read_exact(&mut bytes).unwrap();
        let (sequence, reply) = process::decode_reply(&bytes).unwrap();
        assert_eq!(sequence, self.sequence);
        reply
    }
    fn submit(&mut self, operation: Operation) -> Reply {
        let not_after_ns = fr_native::clock::monotonic_ns()
            .unwrap()
            .checked_add(1_000_000_000)
            .unwrap();
        self.exchange(Request::Submit {
            operation,
            not_after_ns,
        })
    }
    fn stop(&mut self) {
        assert_eq!(self.exchange(Request::Cleanup), Reply::Cleaned(true));
        assert_eq!(self.exchange(Request::Stop), Reply::Stopped);
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Agent {
    fn drop(&mut self) {
        // Only the test-owned child; a failed assertion must not leak an agent
        // or its sharing-indicator connection into another test's server.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn executor_refuses_topology_change_between_prepared_reply_and_submit_command() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut agent = Agent::start(&screen);
    observer.warp(DesktopPoint { x: 400, y: 180 });
    let operation = Operation::Absolute(DesktopPoint { x: 401, y: 181 });
    assert_eq!(agent.exchange(Request::Prepare(operation)), Reply::Prepared);
    screen.remove("fr-selected");
    screen.monitor("fr-selected", 320, 120, 320, 240);
    assert_eq!(
        agent.submit(operation),
        Reply::NotSubmitted(PlatformError::GeometryChanged)
    );
    assert_eq!(observer.pointer().0, DesktopPoint { x: 400, y: 180 });
    assert_eq!(
        agent.exchange(Request::Prepare(operation)),
        Reply::PrepareFailed(PlatformError::GeometryChanged)
    );
    agent.stop();
}

#[test]
fn executor_rechecks_pointer_at_submit_for_prepared_buttons_and_wheel() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut agent = Agent::start(&screen);
    for operation in [
        Operation::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
        Operation::Button {
            button: PointerButton::Back,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Up,
            pressed: true,
        },
        Operation::Wheel {
            direction: WheelDirection::Right,
            pressed: true,
        },
    ] {
        observer.warp(DesktopPoint { x: 400, y: 180 });
        assert_eq!(agent.exchange(Request::Prepare(operation)), Reply::Prepared);
        observer.warp(DesktopPoint { x: 20, y: 30 });
        assert_eq!(
            agent.submit(operation),
            Reply::NotSubmitted(PlatformError::GeometryChanged)
        );
        assert_eq!(observer.pointer().0, DesktopPoint { x: 20, y: 30 });
        assert_eq!(observer.pointer().1 & (256 | 2048), 0);
    }
    assert_eq!(observer.buttons(), [] as [(u32, bool, DesktopPoint); 0]);
    agent.stop();
}

#[test]
fn executor_refuses_an_expired_prepared_operation_after_native_revalidation() {
    let screen = screen();
    let observer = Observer::new(&screen.name);
    let mut agent = Agent::start(&screen);
    observer.warp(DesktopPoint { x: 400, y: 180 });
    let operation = Operation::Absolute(DesktopPoint { x: 401, y: 181 });
    assert_eq!(agent.exchange(Request::Prepare(operation)), Reply::Prepared);
    assert_eq!(
        agent.exchange(Request::Submit {
            operation,
            not_after_ns: 1
        }),
        Reply::Expired
    );
    assert_eq!(observer.pointer().0, DesktopPoint { x: 400, y: 180 });
    assert_eq!(
        agent.submit(operation),
        Reply::NotSubmitted(PlatformError::Unsupported)
    );
    agent.stop();
}
