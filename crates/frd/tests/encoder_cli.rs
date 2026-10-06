#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]
//! Real frd argument/preflight execution, stopping before native or network work.
//! A private, unserved Unix socket is only an existence fixture, NOT Tailscale
//! evidence. Every case asserts that frd made no connection to that socket.
use frd::host_policy::{Approval, Policy};
use serde_json::Value;
use std::{
    fs,
    io::{self, Write},
    os::unix::{
        fs::{DirBuilderExt, OpenOptionsExt},
        net::UnixListener,
    },
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Fixture {
    root: PathBuf,
    policy: Vec<u8>,
    listener: Option<UnixListener>,
}
impl Fixture {
    fn new(approval: Approval, api_exists: bool) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "frd-encoder-cli-{}-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let policy = serde_json::to_vec(&Policy {
            revision: 1,
            approval_mode: approval,
            ..Policy::default()
        })
        .unwrap();
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join("policy.json"))
            .unwrap()
            .write_all(&policy)
            .unwrap();
        let listener = api_exists.then(|| {
            let listener = UnixListener::bind(root.join("api.sock")).unwrap();
            listener.set_nonblocking(true).unwrap();
            listener
        });
        Self { root, policy, listener }
    }
    fn run(&self, extra: &[&str]) -> Value {
        let mut child = Command::new(env!("CARGO_BIN_EXE_frd"))
            .env_clear()
            .args(["run", "--json", "--once", "--config"])
            .arg(self.root.join("policy.json"))
            .arg("--socket")
            .arg(self.root.join("api.sock"))
            .args(["--display", ":0", "--worker"])
            .arg(self.root.join("missing-worker"))
            .args(extra)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= until {
                // Only our still-unreaped child is signalled. There is no
                // process-name lookup, broad kill or re-used PID in this path.
                let _ = child.kill();
                let _ = child.wait();
                panic!("frd preflight did not finish within its test bound");
            }
            thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        let expected_code = if self.listener.is_some() { 2 } else { 1 };
        assert_eq!(output.status.code(), Some(expected_code));
        assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(output.stdout.len() <= 16_384, "bounded preflight diagnostic");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["outcome"], "refusal");
        assert_eq!(fs::read(self.root.join("policy.json")).unwrap(), self.policy);
        if let Some(listener) = &self.listener {
            assert!(matches!(
                listener.accept(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock
            ));
        }
        value
    }
}

#[test]
fn the_real_cli_reaches_native_preflight_for_every_explicit_encoder() {
    let fixture = Fixture::new(Approval::None, true);
    for args in [
        &["--encoder", "nvenc"][..],
        &["--encoder", "vaapi"][..],
        &["--encoder", "software"][..],
        &["--software-explicit"][..],
    ] {
        // Restoring the old software-only guard would fail the hardware cases
        // here. No actual worker, X server, GPU or peer is used by this test.
        assert_eq!(fixture.run(args)["code"], "worker_unavailable", "{args:?}");
    }
}

#[test]
fn missing_ambiguous_or_invalid_encoder_selection_never_starts_the_host() {
    let fixture = Fixture::new(Approval::None, true);
    let absent = fixture.run(&[]);
    assert_eq!(absent["code"], "hardware_hevc_unavailable");
    assert!(absent["detail"].as_str().unwrap().contains("no automatic encoder selection"));
    for args in [
        &["--encoder", "auto"][..],
        &["--encoder", "nvenc", "--software-explicit"][..],
        &["--encoder", "vaapi", "--encoder", "software"][..],
    ] {
        let value = fixture.run(args);
        assert_eq!(value["code"], "invalid_host_policy");
        assert_eq!(value["reason"], "InvalidArgument");
    }
}

#[test]
fn hardware_selection_cannot_bypass_saved_or_explicit_local_approval() {
    let saved = Fixture::new(Approval::Local, true);
    let explicit = Fixture::new(Approval::None, true);
    for encoder in ["software", "nvenc", "vaapi"] {
        assert_eq!(
            saved.run(&["--encoder", encoder])["code"],
            "local_approval_unavailable"
        );
        assert_eq!(
            explicit.run(&["--encoder", encoder, "--approval", "local"])["code"],
            "local_approval_unavailable"
        );
        // Only an explicit operator override can take the unattended path;
        // the saved file remains byte-for-byte unchanged in every invocation.
        assert_eq!(
            saved.run(&["--encoder", encoder, "--approval", "none"])["code"],
            "worker_unavailable"
        );
    }
}

#[test]
fn encoder_selection_does_not_replace_the_installed_tailnet_requirement() {
    let missing = Fixture::new(Approval::None, false);
    for encoder in ["software", "nvenc", "vaapi"] {
        assert_eq!(missing.run(&["--encoder", encoder])["code"], "tailscale_unavailable");
    }
}

#[test]
fn real_cli_accepts_video_rate_targets_for_all_encoders_without_starting_native_work() {
    let fixture = Fixture::new(Approval::None, true);
    for encoder in ["software", "nvenc", "vaapi"] {
        for (fps, bitrate) in [("1", "10000"), ("60", "12000000"), ("240", "200000000")] {
            assert_eq!(
                fixture.run(&["--encoder", encoder, "--fps", fps, "--bitrate", bitrate])["code"],
                "worker_unavailable"
            );
        }
    }
    assert_eq!(fixture.run(&["--fps", "60", "--bitrate", "12000000"])["code"], "hardware_hevc_unavailable");
}

#[test]
fn real_cli_refuses_invalid_or_ambiguous_video_rates_before_native_or_network_work() {
    let fixture = Fixture::new(Approval::None, true);
    for args in [
        &["--fps", "0"][..],
        &["--fps", "241"][..],
        &["--fps", "59.94"][..],
        &["--fps", "60", "--fps", "30"][..],
        &["--bitrate", "9999"][..],
        &["--bitrate", "200000001"][..],
        &["--bitrate", "8M"][..],
        &["--bitrate", "8000000", "--bitrate", "8000000"][..],
    ] {
        let value = fixture.run(args);
        assert_eq!(value["code"], "invalid_host_policy", "{args:?}");
        assert_eq!(value["reason"], "InvalidArgument", "{args:?}");
    }
}

#[test]
fn video_rates_cannot_bypass_local_approval_or_the_installed_tailnet_requirement() {
    let local = Fixture::new(Approval::Local, true);
    let missing = Fixture::new(Approval::None, false);
    let args = ["--encoder", "nvenc", "--fps", "60", "--bitrate", "12000000"];
    assert_eq!(local.run(&args)["code"], "local_approval_unavailable");
    assert_eq!(missing.run(&args)["code"], "tailscale_unavailable");
}
