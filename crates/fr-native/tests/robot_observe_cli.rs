//! Real processes exercise one-shot admission/refusals, not remote media proof.
#![cfg(all(target_os = "linux", feature = "linux-desktop"))]
#![forbid(unsafe_code)]

use std::{
    fs, io,
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-observe-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
struct Process(Option<Child>);
impl Drop for Process {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn run(args: &[&str]) -> Output {
    let mut process = Process(Some(
        Command::new(env!("CARGO_BIN_EXE_fr"))
            .args(args)
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let until = Instant::now() + Duration::from_secs(5);
    while process.0.as_mut().unwrap().try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < until,
            "bounded one-shot CLI did not finish"
        );
        thread::sleep(Duration::from_millis(5));
    }
    let output = process.0.take().unwrap().wait_with_output().unwrap();
    assert!(output.stdout.len() < 16 * 1024);
    assert!(output.stderr.is_empty());
    output
}
fn refused(output: &Output, code: &str, exit: i32) {
    assert_eq!(output.status.code(), Some(exit));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], "refused");
    assert_eq!(value["error"]["code"], code);
    assert!(value.get("data").is_none());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE"));
}

#[test]
fn one_shot_validates_real_local_roots_before_contacting_discovery() {
    let fixture = Fixture::new();
    let roots = fixture.0.join("roots.pem");
    let socket = fixture.0.join("localapi.sock");
    let artifact = fixture.0.join("PRIVATE-screen.png");
    fs::write(&roots, b"PRIVATE-INVALID-CERTIFICATE").unwrap();
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let output = run(&[
        "robot",
        "observe",
        "n-PRIVATE",
        "--native-once",
        "--experimental-native",
        "--display",
        "only",
        "--trust-roots",
        roots.to_str().unwrap(),
        "--socket",
        socket.to_str().unwrap(),
        "--screenshot",
        artifact.to_str().unwrap(),
        "--json",
    ]);
    refused(&output, "invalid_trust_store", 1);
    assert!(!artifact.exists());
    assert!(matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock));
}

#[test]
fn optical_evidence_refuses_before_opening_a_requested_screenshot() {
    let fixture = Fixture::new();
    let artifact = fixture.0.join("PRIVATE-screen.png");
    let args = [
        "robot",
        "observe",
        "n-PRIVATE",
        "--native-once",
        "--experimental-native",
        "--display",
        "only",
        "--screenshot",
        artifact.to_str().unwrap(),
        "--evidence-level",
        "instrumentally_observed",
        "--json",
    ];
    refused(&run(&args), "observation_evidence_unavailable", 2);
    assert!(!artifact.exists());
}

#[test]
fn live_and_local_observation_do_not_silently_enable_pixel_export() {
    let fixture = Fixture::new();
    let artifact = fixture.0.join("PRIVATE-screen.png");
    for mut args in [
        vec![
            "robot",
            "session",
            "open",
            "n-PRIVATE",
            "--foreground",
            "--role",
            "view",
            "--publish-observations",
            "--experimental-native",
            "--display",
            "only",
        ],
        vec![
            "robot",
            "observe",
            "n-PRIVATE",
            "--connection-id",
            "00000000000000000000000000000001",
        ],
    ] {
        args.extend(["--screenshot", artifact.to_str().unwrap(), "--json"]);
        refused(&run(&args), "observation_artifact_unavailable", 2);
        assert!(!artifact.exists());
    }
}

#[test]
fn one_shot_never_accepts_control_files_audio_or_reconnect_options() {
    for flags in [
        vec!["--control"],
        vec!["--send", "/PRIVATE/selected"],
        vec!["--receive", "/PRIVATE/destination"],
        vec!["--audio"],
        vec!["--attempts", "2"],
        vec!["--lease", "PRIVATE-lease"],
    ] {
        let mut args = vec![
            "robot",
            "observe",
            "n-PRIVATE",
            "--native-once",
            "--experimental-native",
            "--display",
            "only",
            "--json",
        ];
        args.extend(flags);
        refused(&run(&args), "invalid_arguments", 2);
    }
}

#[test]
fn legacy_observe_and_unqualified_native_requests_do_not_open_a_new_session() {
    refused(
        &run(&["robot", "observe", "n-PRIVATE", "--json"]),
        "robot_surface_unavailable",
        2,
    );
    refused(
        &run(&[
            "robot",
            "observe",
            "n-PRIVATE",
            "--native-once",
            "--display",
            "only",
            "--json",
        ]),
        "native_transport_unqualified",
        2,
    );
}

#[test]
fn help_explains_new_connection_lifetime_and_unknown_observation_fields() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("--native-once"));
    assert!(help.contains("separate read-only connection"));
    assert!(help.contains("not a headless screenshot"));
    assert!(help.contains("unknown"));
}
