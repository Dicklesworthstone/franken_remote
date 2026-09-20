#![forbid(unsafe_code)]
#![cfg(all(target_os = "linux", feature = "linux-desktop"))]
#![allow(clippy::too_many_lines)]

//! Integration tests for `fr doctor` and `frd status` capability matrix reporting,
//! typed failure explanations, and JSON/human diagnosis output
//! per plan §§2.1, 18.1, 18.4, and 24.5 (bead `fr-p2-doctor-diagnostics-bo4`).

use std::{
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};

fn unique_socket_path(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "fr-doc-{}-{}-{name}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn command(args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_fr"));
    command
        .args(args)
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn wait_for_child(mut child: Child) -> Output {
    let until = Instant::now() + Duration::from_secs(5);
    let mut timed_out = false;
    while child.try_wait().expect("try_wait failed").is_none() {
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!timed_out, "bounded CLI process did not finish");
    let output = child.wait_with_output().expect("wait_with_output failed");
    assert!(output.stdout.len() < 16384, "doctor CLI output bound");
    output
}

#[test]
fn doctor_help_surfaces_matrix_and_diagnostics() {
    let help = wait_for_child(command(&["--help"]).spawn().unwrap());
    assert!(help.status.success());
    let help_text = String::from_utf8_lossy(&help.stdout).to_string();
    assert!(help_text.contains("fr doctor"));
    assert!(help_text.contains("Doctor diagnoses installed Tailscale status, service port collisions, and certificate lifecycle."));
}

#[test]
fn doctor_cli_absent_socket_reports_typed_refusal_json() {
    let missing = unique_socket_path("absent-json");
    let output = wait_for_child(
        command(&["doctor", "--socket", missing.to_str().unwrap(), "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    let json_text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(json_text.contains("\"schema_version\":1"));
    assert!(json_text.contains("\"outcome\":\"refused\""));
    assert!(json_text.contains("\"code\":\"tailscale_unavailable\""));
    assert!(json_text.contains("\"next_action\":"));
}

#[test]
fn doctor_cli_absent_socket_reports_typed_refusal_human() {
    let missing = unique_socket_path("absent-human");
    let output = wait_for_child(
        command(&["doctor", "--socket", missing.to_str().unwrap()])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    let human_text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(human_text.contains("tailscale_unavailable"));
    assert!(
        human_text.contains("Start installed Tailscale and check the protected LocalAPI socket.")
    );
}

#[test]
fn doctor_cli_invalid_arguments_refused() {
    // Port 0 refused
    let output = wait_for_child(
        command(&["doctor", "--port", "0", "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(2));
    let json_text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(json_text.contains("\"code\":\"invalid_arguments\""));

    // Positional arguments refused
    let output = wait_for_child(command(&["doctor", "extra_arg", "--json"]).spawn().unwrap());
    assert_eq!(output.status.code(), Some(2));
    let json_text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(json_text.contains("\"code\":\"invalid_arguments\""));
}
