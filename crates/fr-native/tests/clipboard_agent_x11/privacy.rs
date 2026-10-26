//! Real native privacy surface; the non-XTEST device click is an explicit
//! Xvfb hardware fixture, never evidence of a physical user's consent.
use super::*;
use fr_native::sharing_indicator::{self, Status};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn clipboard_window(display: &str) -> u32 {
    let result = Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/clipboard_agent_x11/window.py"
        ))
        .arg(display)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "independent X11 window lookup failed"
    );
    std::str::from_utf8(&result.stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
fn ui(display: &str, window: u32, op: &str) {
    assert!(
        Command::new("python3")
            .arg(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/sharing_indicator/peer.py"
            ))
            .arg(display)
            .arg(window.to_string())
            .arg(op)
            .status()
            .unwrap()
            .success()
    );
}

#[test]
fn clipboard_turn_off_hiding_and_window_loss_fence_publication_without_stopping_control() {
    for op in ["device-click", "unmap", "destroy"] {
        let server = Server::start();
        let revoked = Arc::new(AtomicBool::new(false));
        let actual = revoked.clone();
        let mut control_ui = sharing_indicator::start_with(&server.display, move |_| {
            actual.store(true, Ordering::Release);
        })
        .unwrap();
        let control = control_ui.control();
        until(Duration::from_secs(3), || {
            control.status() != Status::Opening
        });
        assert!(control.responsive());
        let mut app = X11Clipboard::open(&server.display, &ProtocolLimits::ABSOLUTE, true).unwrap();
        let mine = copy(&mut app, "newer local application copy", 1);
        let mut raw = Raw::start(&server.display);
        assert_eq!(
            raw.exchange(
                Request::Hello {
                    epoch: 91,
                    max_item_bytes: 64
                },
                &[]
            ),
            Reply::Ready { epoch: 91 }
        );
        let window = clipboard_window(&server.display);
        let text = "never publish after privacy off";
        let item = stamp(Endpoint::Controller, 19);
        assert!(matches!(
            raw.exchange(
                Request::Prepare {
                    stamp: item,
                    revision: None,
                    len: u32::try_from(text.len()).unwrap()
                },
                text.as_bytes()
            ),
            Reply::Prepared { .. }
        ));
        ui(&server.display, window, op);
        let mut fixed = [0; 1];
        assert_eq!(
            raw.signals.read(&mut fixed).unwrap(),
            0,
            "UI loss must independently signal frd"
        );
        // The parent has not sent Stop: the child itself must refuse its final
        // publication gate, despite this otherwise valid future deadline.
        raw.send(
            Request::Publish {
                stamp: item,
                not_after_ns: clock::monotonic_ns().unwrap() + 2_000_000_000,
            },
            &[],
        );
        assert_eq!(raw.command.read(&mut fixed).unwrap(), 0);
        assert_eq!(raw.child.wait().unwrap().code(), Some(66));
        app.pump().unwrap();
        assert_eq!(app.current_origin().unwrap(), Some(mine));
        assert!(
            !revoked.load(Ordering::Acquire),
            "clipboard off cannot revoke input"
        );
        assert!(control.responsive());
        control.stop();
        until(Duration::from_secs(2), || control_ui.finish().is_some());
    }
}

#[test]
fn missing_privacy_signal_channel_refuses_before_ready_or_native_access() {
    let server = Server::start();
    let (mut command, theirs) = UnixStream::pair().unwrap();
    command
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut child = Command::new(AGENT)
        .env_clear()
        .env("DISPLAY", &server.display)
        .arg("--clipboard")
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::from(OwnedFd::from(theirs)))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    command
        .write_all(
            &process::encode_request(
                1,
                Request::Hello {
                    epoch: 92,
                    max_item_bytes: 64,
                },
            )
            .unwrap(),
        )
        .unwrap();
    let mut frame = [0; process::FRAME_BYTES];
    command.read_exact(&mut frame).unwrap();
    assert_eq!(
        process::decode_reply(&frame, 64).unwrap(),
        (1, Reply::Refused(PlatformError::Unavailable))
    );
    assert_eq!(child.wait().unwrap().code(), Some(66));
}
