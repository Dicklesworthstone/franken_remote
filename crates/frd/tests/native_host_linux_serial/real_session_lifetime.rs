//! `frd run --logind-session` ends control and the run when the selected
//! session locks (plan 2.2/5.3). The lifecycle source is the EXPLICITLY
//! synthetic session-monitor fixture (`tests/session_monitor/fixture.c`), not
//! logind or a desktop locker: this proves frd's reaction to lock evidence, not
//! that any installed locker reports it faithfully.
use super::real_control::{Controlled, indicator};
use super::shipped_client::wait_for;
use super::*;
use frd::{
    host_run::{self, Event},
    session_monitor::{Error as LifetimeError, State},
};
use std::{
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};

/// The synthetic monitor image, compiled once, not group/other-writable.
fn fixture_image() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("fr-session-lifetime-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let image = dir.join("fixture");
    assert!(
        std::process::Command::new("/usr/bin/cc")
            .args(["-std=c11", "-Wall", "-Wextra", "-Werror"])
            .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/session_monitor/fixture.c"))
            .arg("-o")
            .arg(&image)
            .status()
            .unwrap()
            .success()
    );
    std::fs::set_permissions(&image, std::fs::Permissions::from_mode(0o755)).unwrap();
    image
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real input agent; synthetic session lifecycle"]
fn a_lock_of_the_selected_session_ends_control_and_the_run_with_a_typed_cause() {
    let token = format!("{:032x}", rand_token());
    let marker = PathBuf::from(format!("/tmp/fr-sm-lock-{token}"));
    assert!(!marker.exists());
    let mut s = Controlled::start_monitored(fixture_image(), format!("lockfile-{token}"));

    // Fresh evidence: control works and the executor's indicator is shown.
    assert!(
        s.pointer_follows((200, 150), Duration::from_secs(10)),
        "control before the lock: {}",
        s.daemon.dump()
    );
    assert!(indicator(&mut s.observer).is_some());

    // The selected session locks.
    let locked = Instant::now();
    std::fs::write(&marker, b"").unwrap();

    // Control ends first: the lease's executor (and its indicator) goes away
    // without any further viewer input.
    let mut revoked = None;
    while locked.elapsed() < Duration::from_secs(5) {
        if indicator(&mut s.observer).is_none() {
            revoked = Some(locked.elapsed());
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let revoked =
        revoked.unwrap_or_else(|| panic!("control survived the session lock: {}", s.daemon.dump()));
    // Viewer motion no longer reaches the host.
    assert!(
        !s.pointer_follows((330, 260), Duration::from_secs(2)),
        "input landed after the lock"
    );

    // The run ends by itself, with the specific typed cause.
    let result = s.daemon.ended(Duration::from_secs(20));
    assert_eq!(
        result,
        Err(host_run::Error::SessionEnded(LifetimeError::Native(
            State::Locked
        ))),
        "{}",
        s.daemon.dump()
    );
    assert_eq!(result.unwrap_err().code(), "session_locked");
    let events = s.daemon.dump();
    assert!(events.contains("ShareEnded"), "{events}");
    assert!(!events.contains("CleanupFailed"), "{events}");
    assert!(UdpSocket::bind(address()).is_ok(), "listener retired");
    // The viewer's session ends too.
    let output = wait_for(s.client, Duration::from_secs(30));
    let completion = String::from_utf8_lossy(&output.stdout);
    eprintln!(
        "lock -> executor gone in {revoked:?}; client exit {:?}; completion {}",
        output.status.code(),
        completion.trim()
    );
    // The controller is told why, by the host's authenticated terminal report,
    // not left to guess from a dropped connection.
    let report: serde_json::Value = serde_json::from_str(completion.trim()).unwrap();
    assert_eq!(report["outcome"], "revoked", "{report}");
    assert_eq!(report["error"]["code"], "host_session_ended", "{report}");
    assert_eq!(
        report["error"]["revocation"]["reason"], "session_ended",
        "{report}"
    );
    let _ = std::fs::remove_file(&marker);
    // Event log shape: listening happened before the lock, nothing after stop.
    assert!(
        s.daemon
            .events()
            .iter()
            .any(|e| matches!(e, Event::Listening { .. }))
    );
}

fn rand_token() -> u128 {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).unwrap();
    u128::from_le_bytes(bytes)
}
