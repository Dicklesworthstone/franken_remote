#![cfg(all(target_os = "linux", feature = "linux-input"))]
#![forbid(unsafe_code)]
//! The actual native UI executable over its real inherited socketpairs. A
//! Ready response requires the existing X11 MapNotify/draw path, not a fake
//! sink. This is X11 mapping/protocol evidence, not optical visibility proof.
use fr_core::{
    indicator_process::{FRAME_BYTES, Frame, Kind},
    input_submission::process::{SIGNAL_BYTES, Signal, encode_signal},
};
use std::{
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::{UnixDatagram, UnixStream}, process::CommandExt},
    },
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Running {
    child: Option<Child>,
    command: Option<UnixStream>,
    signals: UnixDatagram,
}
impl Running {
    fn start(display: &str) -> Self {
        let (command, child_command) = UnixStream::pair().unwrap();
        let (signals, child_signals) = UnixDatagram::pair().unwrap();
        command.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        command.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        signals.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut builder = Command::new(env!("CARGO_BIN_EXE_fr-observation-indicator"));
        builder.env_clear().env("DISPLAY", display)
            .arg("--parent-pid").arg(std::process::id().to_string())
            .stdin(Stdio::from(OwnedFd::from(child_command)))
            .stdout(Stdio::from(OwnedFd::from(child_signals)))
            .stderr(Stdio::null()).process_group(0);
        if let Some(path) = std::env::var_os("XAUTHORITY") {
            builder.env("XAUTHORITY", path);
        }
        let child = builder.spawn().unwrap();
        // Closing these inherited-end duplicates makes EOF meaningful.
        drop(builder);
        Self { child: Some(child), command: Some(command), signals }
    }
    fn exchange(&mut self, kind: Kind, sequence: u64) -> Frame {
        let request = Frame { kind, sequence, epoch: 77 };
        let command = self.command.as_mut().unwrap();
        command.write_all(&request.encode().unwrap()).unwrap();
        let mut bytes = [0; FRAME_BYTES];
        command.read_exact(&mut bytes).unwrap();
        let response = Frame::decode(&bytes).unwrap();
        assert!(response.response_to(request).is_ok());
        response
    }
    fn finish(&mut self) -> ExitStatus {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                // No later Drop may signal an already-reaped (reusable) PID.
                self.child = None;
                return status;
            }
            assert!(Instant::now() < until, "original native child failed to exit");
            thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn real_x11_child_maps_then_enforces_protocol_and_parent_lifetime() {
    let display = match std::env::var("DISPLAY") {
        Ok(display) => display,
        Err(_) if std::env::var_os("FR_OBSERVATION_INDICATOR_REQUIRED").is_none() => {
            eprintln!("native observation indicator test requires DISPLAY; not qualified");
            return;
        }
        Err(error) => panic!("required native display missing: {error}"),
    };
    // Sequential: every original child exits before another window is opened.
    for mode in ["input-protocol", "stop", "wrong-sequence", "wrong-epoch", "wrong-direction", "parent-eof", "idle"] {
        let mut native = Running::start(&display);
        if mode == "input-protocol" {
            // The input executor's framing is never an observation-UI command.
            let mut foreign = [0; 64];
            foreign[..4].copy_from_slice(b"FRIA");
            native.command.as_mut().unwrap().write_all(&foreign).unwrap();
            assert_eq!(native.finish().code(), Some(65));
            continue;
        }
        assert_eq!(native.exchange(Kind::Open, 1).kind, Kind::Ready, "actual UI mapping: {mode}");
        assert_eq!(native.exchange(Kind::Check, 2).kind, Kind::Ready, "native progress: {mode}");
        match mode {
            "stop" => {
                assert_eq!(native.exchange(Kind::Stop, 3).kind, Kind::Stopped);
                let mut signal = [0; SIGNAL_BYTES + 1];
                let n = native.signals.recv(&mut signal).unwrap();
                assert_eq!(&signal[..n], &encode_signal(Signal::LocalRevoke));
                assert!(native.finish().success());
            }
            "wrong-sequence" | "wrong-epoch" | "wrong-direction" => {
                let request = Frame {
                    kind: if mode == "wrong-direction" { Kind::Ready } else { Kind::Check },
                    sequence: if mode == "wrong-sequence" { 2 } else { 3 },
                    epoch: if mode == "wrong-epoch" { 78 } else { 77 },
                };
                native.command.as_mut().unwrap().write_all(&request.encode().unwrap()).unwrap();
                assert_eq!(native.finish().code(), Some(65), "{mode}");
            }
            "parent-eof" => {
                drop(native.command.take());
                assert_eq!(native.finish().code(), Some(67));
            }
            "idle" => {
                // No incoming heartbeat: a parent that abandoned this child
                // cannot leave a seemingly live read-only indicator forever.
                assert_eq!(native.finish().code(), Some(67));
            }
            _ => unreachable!("fixture mode"),
        }
    }
}
