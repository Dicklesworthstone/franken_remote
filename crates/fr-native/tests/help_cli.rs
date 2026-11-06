//! The shipped CLI's static help needs neither native SDK features nor a
//! display, Tailscale connection, local session owner or platform service.
#![cfg(feature = "cli")]
#![forbid(unsafe_code)]

use std::{
    io::Read,
    process::{Command, Output, Stdio},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const MAX_HELP_BYTES: usize = 16 * 1024;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fr"));
    command
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .env_remove("DBUS_SESSION_BUS_ADDRESS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn read(pipe: impl Read + Send + 'static) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        pipe.take((MAX_HELP_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .unwrap();
        bytes
    })
}

fn run_command(command: &mut Command) -> Output {
    let mut child = command.spawn().unwrap();
    // Drain while waiting: a valid help page may exceed an OS pipe's capacity.
    let stdout = read(child.stdout.take().unwrap());
    let stderr = read(child.stderr.take().unwrap());
    let until = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("static help did not finish within its process budget");
        }
        thread::sleep(Duration::from_millis(5));
    };
    let output = Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    };
    assert!(output.stdout.len() < MAX_HELP_BYTES);
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {:?}",
        output.stderr
    );
    output
}

fn run(args: &[&str]) -> Output {
    run_command(command().args(args))
}

#[test]
fn overview_is_bounded_and_discovers_the_detailed_topics() {
    let overview = run(&[]);
    assert!(overview.status.success());
    let text = String::from_utf8(overview.stdout.clone()).unwrap();
    for topic in [
        "connect",
        "robot",
        "robot session",
        "robot observe",
        "robot input",
        "send",
    ] {
        assert!(text.contains(&format!("fr help {topic}")));
    }
    assert!(text.contains("fr send NODE_ID"));
    assert!(text.contains("not a remote session or input lease"));
    for args in [&["--help"][..], &["help"][..], &["help", "--help"]] {
        let output = run(args);
        assert!(output.status.success());
        assert_eq!(output.stdout, overview.stdout);
    }
}

#[test]
fn detailed_topics_are_reachable_before_any_operational_dispatch() {
    for (topic, detail) in [
        (&["connect"][..], "--receive-max-session-bytes"),
        (&["hosts", "save"][..], "Private atomic storage"),
        (&["robot"][..], "fr help robot observe"),
        (&["robot", "session", "close"][..], "local_session_changed"),
        (
            &["robot", "observe"][..],
            "Receipt age is local metadata age",
        ),
        (&["robot", "input"][..], "--observation-id INTEGER"),
        (&["send"][..], "pacing_changed_by_command"),
    ] {
        let mut args = vec!["help"];
        args.extend_from_slice(topic);
        let output = run(&args);
        assert!(output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains(detail));
        let mut alias = topic.to_vec();
        alias.push("--help");
        let aliased = run(&alias);
        assert!(aliased.status.success());
        assert_eq!(aliased.stdout, output.stdout);
    }
}

fn invalid(args: &[&str]) -> serde_json::Value {
    let output = run(args);
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["outcome"], "refused");
    assert_eq!(value["error"]["code"], "invalid_arguments");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE-ARGUMENT"));
    value
}

#[test]
fn malformed_help_refuses_without_echoing_targets_or_running_commands() {
    for args in [
        &["help", "PRIVATE-ARGUMENT", "--json"][..],
        &["help", "robot", "input", "PRIVATE-ARGUMENT", "--json"][..],
        &["robot", "observe", "PRIVATE-ARGUMENT", "--json", "--help"][..],
        &[
            "connect",
            "PRIVATE-ARGUMENT",
            "--view-only",
            "--json",
            "--help",
        ][..],
        &["send", "PRIVATE-ARGUMENT", "--json", "--help"][..],
        &["robot", "--help", "input", "--json"][..],
        &["--help", "--json"][..],
        &["help", "robot\ninput", "--json"][..],
    ] {
        assert!(
            invalid(args)["error"]["next_action"]
                .as_str()
                .unwrap()
                .contains("fr help TOPIC")
        );
    }
}

#[test]
fn help_does_not_bypass_original_argument_count_or_size_limits() {
    let mut many = vec!["--json", "help"];
    many.extend(std::iter::repeat_n("PRIVATE-ARGUMENT", 31));
    let large = format!(
        "PRIVATE-ARGUMENT{}",
        "x".repeat(4097 - "PRIVATE-ARGUMENT".len())
    );
    for refusal in [invalid(&many), invalid(&["--json", "help", &large])] {
        assert!(
            refusal["error"]["next_action"]
                .as_str()
                .unwrap()
                .contains("argument limits exceeded")
        );
    }
}

#[cfg(unix)]
#[test]
fn help_does_not_bypass_utf8_validation() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let output = run_command(command().args([
        OsString::from("--json"),
        OsString::from("help"),
        OsString::from_vec(vec![0xff]),
    ]));
    assert_eq!(output.status.code(), Some(2));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "invalid_arguments");
    assert_eq!(value["error"]["next_action"], "Use UTF-8 arguments.");
}

#[cfg(target_os = "linux")]
#[test]
fn topic_help_preserves_output_failure_exit_status() {
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    let output = command()
        .args(["help", "robot", "input"])
        .stdout(full)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(74));
    assert_eq!(output.stderr, [] as [u8; 0]);
}
