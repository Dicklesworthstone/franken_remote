//! `frd install` on macOS, through the real binary with a private `HOME`: the
//! per-user `LaunchAgent` accepts exactly what `frd run` on macOS accepts, is
//! published privately and atomically, and `frd uninstall` removes only it.
//! Nothing here loads the agent into launchd.
#![cfg(all(target_os = "macos", host_stack))]
#![forbid(unsafe_code)]

use std::{
    fs,
    net::UdpSocket,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Output},
};

const LABEL_FILE: &str = "com.frankenremote.frd.plist";

/// A private HOME under the real (not /var-symlinked) temporary directory;
/// the installer's no-follow walk refuses symlinked ancestors by design.
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).unwrap();
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "frd-launchd-home-{:032x}",
            u128::from_ne_bytes(random)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn agent(&self) -> PathBuf {
        self.0.join("Library/LaunchAgents").join(LABEL_FILE)
    }
    fn frd(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_frd"))
            .args(args)
            .env("HOME", &self.0)
            .output()
            .unwrap()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn free_port() -> String {
    let probe = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
    probe.local_addr().unwrap().port().to_string()
}

fn json(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "not JSON: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

/// The refusal code of a `--dry-run --json` install with these arguments, or
/// None when it is accepted.
fn refusal(home: &Home, args: &[&str]) -> Option<String> {
    let mut command = vec!["install", "--dry-run", "--json"];
    command.extend_from_slice(args);
    let output = home.frd(&command);
    let report = json(&output);
    if output.status.success() {
        assert_eq!(report["outcome"], "success");
        return None;
    }
    assert_eq!(output.status.code(), Some(2), "{report}");
    assert_eq!(report["outcome"], "refusal");
    Some(report["code"].as_str().unwrap().to_owned())
}

fn program_arguments(plist: &str) -> Vec<String> {
    plist
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .and_then(|rest| rest.split("</array>").next())
        .unwrap()
        .lines()
        .filter_map(|line| {
            line.trim()
                .strip_prefix("<string>")?
                .strip_suffix("</string>")
        })
        .map(str::to_owned)
        .collect()
}

#[test]
fn an_agent_needs_an_explicit_encoder_and_a_daemon_is_refused() {
    let home = Home::new();
    assert_eq!(
        refusal(&home, &[]).as_deref(),
        Some("hardware_hevc_unavailable")
    );
    for encoder in ["videotoolbox", "auto"] {
        assert_eq!(refusal(&home, &["--", "--encoder", encoder]), None);
    }
    // A LaunchDaemon has no login session's window server.
    assert_eq!(
        refusal(&home, &["--system", "--", "--encoder", "videotoolbox"]).as_deref(),
        Some("platform_unavailable")
    );
    // Two selectors are a contradiction, never a preference order.
    assert_eq!(
        refusal(
            &home,
            &["--software-explicit", "--", "--encoder", "videotoolbox"]
        )
        .as_deref(),
        Some("invalid_service_options")
    );
    assert!(!home.0.join("Library").exists(), "a preview writes nothing");
}

#[test]
fn capabilities_without_a_macos_backend_refuse_with_frd_runs_own_codes() {
    let home = Home::new();
    for (tail, code) in [
        (&["--audio"][..], "audio_server_unavailable"),
        (&["--headless"][..], "no_display"),
        (&["--display", ":1"][..], "no_display"),
        (
            &["--logind-session", "c2"][..],
            "session_monitor_unavailable",
        ),
        // The lease-bound features still need the executor first.
        (&["--clipboard"][..], "clipboard_requires_input_agent"),
    ] {
        let mut args = vec!["--", "--encoder", "videotoolbox"];
        args.extend_from_slice(tail);
        assert_eq!(refusal(&home, &args).as_deref(), Some(code), "{tail:?}");
    }
    assert_eq!(
        refusal(
            &home,
            &["--approval", "local", "--", "--encoder", "videotoolbox"]
        )
        .as_deref(),
        Some("local_approval_unavailable")
    );
}

#[test]
fn installer_owned_and_one_shot_flags_are_not_run_options() {
    let home = Home::new();
    for tail in [
        &["--port", "9000"][..],
        &["--approval", "none"][..],
        &["--socket", "/var/run/tailscaled.socket"][..],
        &["--once"][..],
        &["--input-agent", "relative/fr-input-agent"][..],
        &["--no-such-flag"][..],
    ] {
        let mut args = vec!["--", "--encoder", "videotoolbox"];
        args.extend_from_slice(tail);
        assert_eq!(
            refusal(&home, &args).as_deref(),
            Some("invalid_service_options"),
            "{tail:?}"
        );
    }
}

#[test]
fn install_publishes_a_private_agent_and_uninstall_removes_only_it() {
    let home = Home::new();
    let port = free_port();
    let agent_path = "/Users/u/Library/Application Support/FrankenRemote/bin/fr-input-agent";
    let install = |approval: &str| {
        home.frd(&[
            "install",
            "--json",
            "--port",
            &port,
            "--approval",
            approval,
            "--socket",
            "/var/run/tailscaled.socket",
            "--",
            "--encoder",
            "videotoolbox",
            "--input-agent",
            agent_path,
        ])
    };
    let output = install("none");
    assert!(output.status.success(), "{}", json(&output));
    let report = json(&output);
    assert_eq!(report["kind"], "launchd-agent");
    assert_eq!(report["started"], false);
    assert_eq!(report["durable"], true);
    assert_eq!(
        Path::new(report["unit_path"].as_str().unwrap()),
        home.agent()
    );
    let steps = report["next_steps"].to_string();
    assert!(steps.contains("launchctl bootstrap gui/"), "{steps}");
    assert!(!steps.contains("view-only"), "{steps}");

    let plist = fs::read_to_string(home.agent()).unwrap();
    let metadata = fs::symlink_metadata(home.agent()).unwrap();
    assert!(metadata.file_type().is_file());
    assert_eq!(metadata.mode() & 0o077, 0, "a private plist");
    let arguments = program_arguments(&plist);
    assert_eq!(arguments[0], env!("CARGO_BIN_EXE_frd"));
    assert_eq!(
        arguments[1..],
        [
            "run",
            "--port",
            &port,
            "--socket",
            "/var/run/tailscaled.socket",
            "--approval",
            "none",
            "--encoder",
            "videotoolbox",
            "--input-agent",
            agent_path,
        ]
    );
    // Exactly what frd run's own parser accepts.
    let parsed = frd::host_policy::options::RunOptions::parse(&arguments[2..]).unwrap();
    assert!(parsed.clipboard, "the installed control role defaults on");
    // The log goes to the user's own 0700 directory, never a shared /tmp.
    let logs = home.0.join("Library/Logs/FrankenRemote");
    assert!(plist.contains(&format!(
        "<string>{}</string>",
        logs.join("frd.log").display()
    )));
    assert!(!plist.contains("<string>/tmp/frd"));
    assert_eq!(fs::metadata(&logs).unwrap().mode() & 0o777, 0o700);

    // A second install replaces the whole definition atomically.
    let foreign = home.0.join("Library/LaunchAgents/com.example.other.plist");
    fs::write(&foreign, b"another agent").unwrap();
    assert!(install("none").status.success());
    assert_eq!(fs::read_to_string(home.agent()).unwrap(), plist);

    let removed = home.frd(&["uninstall", "--json"]);
    assert!(removed.status.success());
    let removed = json(&removed);
    assert_eq!(removed["existed"], true);
    assert!(
        removed["next_steps"]
            .to_string()
            .contains("launchctl bootout gui/")
    );
    assert!(!home.agent().exists());
    assert_eq!(json(&home.frd(&["uninstall", "--json"]))["existed"], false);
    assert_eq!(fs::read(&foreign).unwrap(), b"another agent");
    let stages: Vec<_> = fs::read_dir(home.0.join("Library/LaunchAgents"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(
        stages,
        [std::ffi::OsString::from("com.example.other.plist")]
    );
}

#[test]
fn installed_clipboard_choices_roundtrip_through_the_real_run_parser() {
    let home = Home::new();
    let port = free_port();
    for (choice, enabled) in [
        (None, true),
        (Some("--clipboard"), true),
        (Some("--no-clipboard"), false),
    ] {
        let mut args = vec![
            "install",
            "--json",
            "--port",
            &port,
            "--",
            "--encoder",
            "videotoolbox",
            "--input-agent",
            "/opt/fr/fr-input-agent",
        ];
        if let Some(choice) = choice {
            args.push(choice);
        }
        let output = home.frd(&args);
        assert!(output.status.success(), "{}", json(&output));
        let plist = fs::read_to_string(home.agent()).unwrap();
        let arguments = program_arguments(&plist);
        let parsed = frd::host_policy::options::RunOptions::parse(&arguments[2..]).unwrap();
        assert_eq!(parsed.clipboard, enabled);
        assert_eq!(
            parsed.macos_refusal(frd::host_policy::Approval::parse("none").unwrap()),
            None
        );
        if let Some(choice) = choice {
            assert!(arguments.iter().any(|argument| argument == choice));
        }
    }
}

#[test]
fn a_symlinked_launch_agents_directory_is_refused_without_writing() {
    let home = Home::new();
    let elsewhere = home.0.join("elsewhere");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&elsewhere)
        .unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(home.0.join("Library"))
        .unwrap();
    std::os::unix::fs::symlink(&elsewhere, home.0.join("Library/LaunchAgents")).unwrap();
    let output = home.frd(&[
        "install",
        "--json",
        "--port",
        &free_port(),
        "--",
        "--encoder",
        "videotoolbox",
    ]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["outcome"], "failure");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}
