//! Real foreground CLI entry points and refusal ordering, not live-host proof.
#![cfg(all(target_os = "linux", feature = "linux-desktop"))]
#![forbid(unsafe_code)]

use std::{
    fs,
    io,
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-robot-foreground-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.0.join("roots.pem"));
        let _ = fs::remove_file(self.0.join("localapi.sock"));
        let _ = fs::remove_dir(&self.0);
    }
}
fn run(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fr"))
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("foreground CLI refusal did not finish within its test bound");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.len() < 8192);
    assert!(output.stderr.is_empty());
    output
}
fn refused(output: &Output, code: &str, exit: i32) {
    assert_eq!(output.status.code(), Some(exit));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], "refused");
    assert_eq!(value["error"]["code"], code);
    assert!(value.get("lease").is_none());
    assert!(value.get("control_granted").is_none());
}

#[test]
fn both_foreground_roles_reach_real_trust_validation_before_network_or_native_work() {
    let fixture = Fixture::new();
    let roots = fixture.0.join("roots.pem");
    let socket = fixture.0.join("localapi.sock");
    fs::write(&roots, b"PRIVATE-INVALID-CERTIFICATE").unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    for role in ["view", "control"] {
        let output = run(&[
            "robot",
            "session",
            "open",
            "n-private",
            "--foreground",
            "--role",
            role,
            "--experimental-native",
            "--display",
            "only",
            "--worker",
            "/absent/fr-robot-foreground-worker",
            "--trust-roots",
            roots.to_str().unwrap(),
            "--socket",
            socket.to_str().unwrap(),
            "--json",
        ]);
        refused(&output, "invalid_trust_store", 1);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("PRIVATE-INVALID-CERTIFICATE"));
        assert!(!text.contains("n-private"));
        assert!(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
    }
}

#[test]
fn foreground_view_cannot_obtain_control_only_capabilities_through_the_robot_entrypoint() {
    for (flags, code) in [
        (vec!["--clipboard"], "clipboard_requires_control"),
        (vec!["--send", "/private/selected"], "send_requires_control"),
        (vec!["--receive", "/private/destination"], "receive_requires_control"),
        (vec!["--control"], "connection_role_required"),
    ] {
        let mut args = vec![
            "robot",
            "session",
            "open",
            "n-private",
            "--foreground",
            "--role",
            "view",
            "--experimental-native",
            "--display",
            "only",
            "--json",
        ];
        args.extend(flags);
        let output = run(&args);
        refused(&output, code, 2);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("/private/"));
    }
}

#[test]
fn legacy_broker_open_stays_unavailable_and_does_not_become_a_foreground_session() {
    let output = run(&[
        "robot", "session", "open", "n-private", "--role", "view", "--json",
    ]);
    refused(&output, "robot_surface_unavailable", 2);
}

#[test]
fn explicit_foreground_role_still_requires_native_transport_opt_in() {
    for role in ["view", "control"] {
        let output = run(&[
            "robot",
            "session",
            "open",
            "n-private",
            "--foreground",
            "--role",
            role,
            "--json",
        ]);
        refused(&output, "native_transport_unqualified", 2);
    }
}
