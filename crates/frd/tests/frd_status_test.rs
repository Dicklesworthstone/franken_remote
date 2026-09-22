#![forbid(unsafe_code)]
#![allow(clippy::too_many_lines)]

//! Integration tests for host daemon status, typed failure explanations,
//! and capability matrix reporting per plan §§2.1, 18.1, 18.4, and 24.5
//! (bead `fr-p2-doctor-diagnostics-bo4`).
//!
//! Asserts:
//! - Golden JSON/human representation for representative states
//! - Exact reproduction of typed failure codes (`permission_revoked`, `cert_expired`, `no_hardware_encoder`)
//! - Untested capabilities are strictly "not tested", never "supported with caveats"
//! - Every refusal emits its typed code to the structured log trail
//! - Schema-versioned robot envelope (`fr.status.v1`)

use frd::status::{CapabilityStatus, DaemonStatusReport};
use std::fs;
use std::path::PathBuf;

fn fixture_path(filename: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("doctor")
        .join(filename)
}

fn assert_or_write_fixture(filename: &str, content: &str) {
    let path = fixture_path(filename);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("failed to create fixture directory");
    }

    if path.exists() {
        let expected = fs::read_to_string(&path).expect("failed to read fixture");
        assert_eq!(
            content, expected,
            "Fixture mismatch for {filename}. If intentional, update golden fixtures."
        );
    } else {
        fs::write(&path, content).expect("failed to write fixture");
    }
}

#[test]
fn nominal_status_operational_contract() {
    let report = DaemonStatusReport::nominal_operational();

    // 1. Overall outcome
    assert_eq!(report.schema_version, "fr.status.v1");
    assert_eq!(report.outcome, "success");
    assert_eq!(report.refusal_code, None);
    assert_eq!(report.next_action, None);

    // 2. Tailscale connectivity & metadata
    assert_eq!(report.tailscale.variant, "standalone");
    assert_eq!(report.tailscale.tailnet, "example.ts.net");
    assert!(report.tailscale.connected);
    assert_eq!(report.tailscale.node_id, "node-101");
    assert_eq!(report.tailscale.node_name, "workstation-alpha");
    assert_eq!(report.tailscale.user_id, "user-101");
    assert_eq!(
        report.tailscale.addresses,
        vec!["100.64.0.1", "fd7a:115c:a1e0::1"]
    );

    // 3. Certificate lifecycle
    assert_eq!(
        report.certificate.certificate_name,
        "workstation-alpha.example.ts.net"
    );
    assert!(report.certificate.valid);
    assert_eq!(report.certificate.generation, 1);
    assert_eq!(report.certificate.expiry_countdown_secs, Some(7_776_000));
    assert_eq!(
        report.certificate.status_message,
        "Certificate active and trusted by local roots"
    );

    // 4. Permissions (all granted in nominal state)
    let perm_caps: Vec<&str> = report.permissions.iter().map(|p| p.capability).collect();
    assert_eq!(
        perm_caps,
        vec![
            "screen_capture",
            "input_injection",
            "clipboard_sync",
            "audio_playback",
            "audio_microphone",
            "file_transfer"
        ]
    );
    for p in &report.permissions {
        assert_eq!(
            p.status, "granted",
            "permission for {} should be granted",
            p.capability
        );
    }

    // 5. Capability Matrix
    let cap_names: Vec<&str> = report.capabilities.iter().map(|c| c.name).collect();
    for name in [
        "video_encode",
        "video_decode",
        "screen_capture",
        "input_injection",
        "clipboard_sync",
        "audio_playback",
        "audio_microphone",
        "file_transfer",
    ] {
        assert!(cap_names.contains(&name));
    }

    for c in &report.capabilities {
        assert_eq!(
            c.status,
            CapabilityStatus::Passed,
            "capability {} should pass in nominal state",
            c.name
        );
    }
    let encode_cap = report
        .capabilities
        .iter()
        .find(|c| c.name == "video_encode")
        .unwrap();
    assert_eq!(encode_cap.hardware_accelerated, Some(true));

    // 6. Active sessions & sharing
    assert_eq!(report.sessions.len(), 1);
    let s = &report.sessions[0];
    assert_eq!(s.session_id, 101);
    assert_eq!(s.device_name, "laptop-controller");
    assert_eq!(s.role, "controller");
    assert_eq!(s.authority_state, "admitted");

    assert_eq!(report.sharing.sharing_scope, "own-user");
    assert_eq!(report.sharing.approval_mode, "unattended");
    assert_eq!(report.sharing.active_viewers, 1);
    assert_eq!(report.sharing.max_viewers, 3);
    assert_eq!(report.sharing.active_controller, Some(101));

    // 7. Restrictions
    assert_eq!(report.restrictions.len(), 6);
    let restr_codes: Vec<&str> = report.restrictions.iter().map(|r| r.code).collect();
    for code in [
        "video_hevc_only",
        "audio_opus_only",
        "full_display_only",
        "tailscale_ingress_only",
        "single_controller_authority",
        "no_zero_rtt_application_data",
    ] {
        assert!(restr_codes.contains(&code));
    }

    // 8. Structured log
    assert_eq!(report.structured_logs.len(), 1);
    assert_eq!(report.structured_logs[0].level, "info");
    assert_eq!(report.structured_logs[0].event_code, "status_check");
    assert_eq!(
        report.structured_logs[0].message,
        "Host daemon operational and healthy"
    );

    // 9. Golden fixtures
    let json_render = report.render_json();
    assert!(json_render.contains("\"schema_version\":\"fr.status.v1\""));
    assert!(json_render.contains("\"outcome\":\"success\""));
    assert!(json_render.contains("\"refusal_code\":null"));
    assert!(json_render.contains("\"next_action\":null"));
    assert_or_write_fixture("nominal_operational.json", &json_render);

    let human_render = report.render_human();
    for s in [
        "Status: [OK - OPERATIONAL]",
        "1. TAILSCALE NETWORK STATUS:",
        "2. TLS 1.3 CERTIFICATE LIFECYCLE:",
        "3. OS PERMISSION STATES:",
        "4. HARDWARE & MEDIA CAPABILITY MATRIX:",
        "5. ACTIVE SESSIONS & AUTHORITY:",
        "6. KNOWN RESTRICTIONS & SCOPE LIMITS:",
        "7. STRUCTURED DIAGNOSTIC LOG TRAIL:",
    ] {
        assert!(human_render.contains(s));
    }
    assert_or_write_fixture("nominal_operational.txt", &human_render);
}

fn assert_refusal_fixtures(
    report: &DaemonStatusReport,
    code: &str,
    action: &str,
    log_msg: &str,
    fixture_stem: &str,
    json_extra: &[&str],
    human_extra: &[&str],
) {
    assert_eq!(report.outcome, "refusal");
    assert_eq!(report.refusal_code, Some(code));
    assert_eq!(report.next_action, Some(action));
    let err_log = report
        .structured_logs
        .iter()
        .find(|l| l.event_code == code)
        .expect("log");
    assert_eq!(err_log.level, "error");
    assert_eq!(err_log.message, log_msg);
    let json_render = report.render_json();
    assert!(json_render.contains(&format!("\"refusal_code\":\"{code}\"")));
    for s in json_extra {
        assert!(json_render.contains(s), "missing json snippet {s}");
    }
    assert_or_write_fixture(&format!("{fixture_stem}.json"), &json_render);
    let human_render = report.render_human();
    assert!(human_render.contains("Status: [REFUSAL - ACTION REQUIRED]"));
    assert!(human_render.contains(&format!("Refusal Code: {code}")));
    for s in human_extra {
        assert!(human_render.contains(s), "missing human snippet {s}");
    }
    assert_or_write_fixture(&format!("{fixture_stem}.txt"), &human_render);
}

#[test]
fn permission_revoked_failure_reproduction() {
    let report = DaemonStatusReport::permission_revoked("screen_capture");
    let perm = report
        .permissions
        .iter()
        .find(|p| p.capability == "screen_capture")
        .unwrap();
    assert_eq!(perm.status, "denied");
    assert!(
        perm.detail
            .as_ref()
            .unwrap()
            .contains("denied or revoked by operator")
    );
    for name in ["screen_capture", "video_encode"] {
        let cap = report.capabilities.iter().find(|c| c.name == name).unwrap();
        assert_eq!(cap.status, CapabilityStatus::Blocked);
        assert!(cap.detail.contains("permission denied by host OS"));
    }
    assert_refusal_fixtures(
        &report,
        "permission_missing",
        "Grant required permission in OS System Settings under Privacy & Security, then restart service.",
        "Refusal: screen_capture permission is not granted by the host OS",
        "permission_revoked",
        &[
            "\"capability\":\"screen_capture\",\"status\":\"denied\"",
            "\"name\":\"screen_capture\",\"status\":\"blocked\"",
        ],
        &["[DENIED]", "[BLOCKED]"],
    );
}

#[test]
fn certificate_expired_failure_reproduction() {
    let report = DaemonStatusReport::certificate_expired();
    assert!(!report.certificate.valid);
    assert_eq!(report.certificate.expiry_countdown_secs, Some(0));
    assert_eq!(
        report.certificate.status_message,
        "Certificate expired 120 seconds ago"
    );
    for c in &report.capabilities {
        assert_eq!(c.status, CapabilityStatus::Blocked);
    }
    assert_refusal_fixtures(
        &report,
        "certificate_expired",
        "Run 'tailscale cert <machine>' to provision a fresh HTTPS certificate, or check Tailscale daemon status.",
        "Refusal: Host Tailscale certificate has expired; ingress connections refused",
        "certificate_expired",
        &["\"valid\":false", "\"expiry_countdown_secs\":0"],
        &["EXPIRED / INVALID"],
    );
}

#[test]
fn no_hardware_encoder_failure_reproduction() {
    let report = DaemonStatusReport::no_hardware_encoder();
    let enc = report
        .capabilities
        .iter()
        .find(|c| c.name == "video_encode")
        .unwrap();
    assert_eq!(enc.status, CapabilityStatus::Failed);
    assert_eq!(enc.hardware_accelerated, Some(false));
    assert_eq!(
        enc.detail,
        "No supported HEVC hardware encoder found on host GPU"
    );
    let dec = report
        .capabilities
        .iter()
        .find(|c| c.name == "video_decode")
        .unwrap();
    assert_eq!(dec.status, CapabilityStatus::Passed);
    assert_refusal_fixtures(
        &report,
        "no_hardware_encoder",
        "Install hardware HEVC drivers (e.g. VAAPI / NVENC / QuickSync) or enable hardware acceleration in system BIOS.",
        "Refusal: Probed video encoder candidate rejected; hardware HEVC is required by plan §3.3",
        "no_hardware_encoder",
        &[
            "\"name\":\"video_encode\",\"status\":\"failed\"",
            "\"hardware_accelerated\":false",
        ],
        &["[FAILED]", "[SW]"],
    );
}

#[test]
fn tailnet_disconnected_failure_reproduction() {
    let report = DaemonStatusReport::tailnet_disconnected();
    assert!(!report.tailscale.connected);
    for c in &report.capabilities {
        assert_eq!(c.status, CapabilityStatus::Blocked);
    }
    assert_refusal_fixtures(
        &report,
        "tailnet_disconnected",
        "Connect Tailscale via 'tailscale up' or start the tailscaled daemon.",
        "Refusal: Tailscale client is not connected to any tailnet node",
        "tailnet_disconnected",
        &["\"connected\":false"],
        &["DISCONNECTED"],
    );
}

#[test]
fn untested_capability_rule_adherence() {
    assert_eq!(CapabilityStatus::NotTested.as_str(), "not tested");
    assert_eq!(CapabilityStatus::NotTested.badge(), "[NOT TESTED]");
    let mut report = DaemonStatusReport::nominal_operational();
    report.capabilities[0].status = CapabilityStatus::NotTested;
    report.capabilities[0].detail = "Untested codec candidate".into();
    let json = report.render_json();
    assert!(json.contains("\"status\":\"not tested\""));
    assert!(
        !json.to_lowercase().contains("supported with caveats")
            && !json.to_lowercase().contains("caveat")
    );
    let human = report.render_human();
    assert!(human.contains("[NOT TESTED]"));
    assert!(
        !human.to_lowercase().contains("supported with caveats")
            && !human.to_lowercase().contains("caveat")
    );
}

fn frd_command(args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_frd"));
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command
}

fn wait_for_child(mut child: std::process::Child) -> std::process::Output {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut timed_out = false;
    while child.try_wait().expect("try_wait failed").is_none() {
        if std::time::Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!timed_out, "bounded CLI process did not finish");
    let output = child.wait_with_output().expect("wait_with_output failed");
    assert!(output.stdout.len() < 16384, "frd CLI output bound");
    output
}

#[test]
fn frd_status_binary_nominal_json_contract() {
    let output = wait_for_child(frd_command(&["status", "--json"]).spawn().unwrap());
    assert!(output.status.success());
    let json_text = String::from_utf8_lossy(&output.stdout).to_string();

    let required_json_snippets = "\
\"schema_version\":\"fr.status.v1\"
\"outcome\":\"success\"
\"refusal_code\":null
\"next_action\":null
\"variant\":\"standalone\"
\"tailnet\":\"example.ts.net\"
\"connected\":true
\"node_name\":\"workstation-alpha\"
\"certificate_name\":\"workstation-alpha.example.ts.net\"
\"valid\":true
\"expiry_countdown_secs\":7776000
\"permissions\":[
\"capability\":\"screen_capture\",\"status\":\"granted\"
\"capability\":\"input_injection\",\"status\":\"granted\"
\"capability\":\"clipboard_sync\",\"status\":\"granted\"
\"capability\":\"audio_playback\",\"status\":\"granted\"
\"capability\":\"audio_microphone\",\"status\":\"granted\"
\"capability\":\"file_transfer\",\"status\":\"granted\"
\"capabilities\":[
\"name\":\"video_encode\",\"status\":\"passed\"
\"name\":\"video_decode\",\"status\":\"passed\"
\"hardware_accelerated\":true
\"sharing_scope\":\"own-user\"
\"approval_mode\":\"unattended\"
\"sessions\":[
\"role\":\"controller\"
\"restrictions\":[
\"code\":\"video_hevc_only\"
\"code\":\"audio_opus_only\"
\"code\":\"full_display_only\"
\"code\":\"tailscale_ingress_only\"
\"code\":\"single_controller_authority\"
\"code\":\"no_zero_rtt_application_data\"
\"structured_logs\":[
\"event_code\":\"status_check\"";

    for s in required_json_snippets.lines() {
        assert!(json_text.contains(s), "missing json string: {s}");
    }
}

#[test]
fn frd_status_binary_nominal_human_contract() {
    let output = wait_for_child(frd_command(&["status"]).spawn().unwrap());
    assert!(output.status.success());
    let human_text = String::from_utf8_lossy(&output.stdout).to_string();

    let required_human_snippets = "\
FRANKENREMOTE HOST DAEMON STATUS (frd)
Status: [OK - OPERATIONAL]
1. TAILSCALE NETWORK STATUS:
2. TLS 1.3 CERTIFICATE LIFECYCLE:
3. OS PERMISSION STATES:
4. HARDWARE & MEDIA CAPABILITY MATRIX:
5. ACTIVE SESSIONS & AUTHORITY:
6. KNOWN RESTRICTIONS & SCOPE LIMITS:
7. STRUCTURED DIAGNOSTIC LOG TRAIL:
screen_capture     [GRANTED]
video_encode       [PASSED]       [HW]
video_hevc_only
own-user";

    for s in required_human_snippets.lines() {
        assert!(human_text.contains(s), "missing human string: {s}");
    }
}

#[test]
#[cfg(target_os = "linux")]
#[rustfmt::skip]
fn frd_approval_and_sharing_cli_commands() {
    use std::os::unix::fs::DirBuilderExt;
    struct TestDirectory(PathBuf);
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let directory = TestDirectory(
        std::env::temp_dir().join(format!("frd-policy-cli-{}-{unique}", std::process::id())),
    );
    fs::DirBuilder::new().mode(0o700).create(&directory.0).unwrap();
    let path = directory.0.join("policy.json");
    let frd_command = |args: &[&str]| {
        let mut command = frd_command(args);
        command.arg("--config").arg(&path);
        command
    };

    let cases: &[(&[&str], &str)] = &[
        (&["approval", "get"], "Approval Mode: unattended"),
        (&["approval", "get", "--json"], "\"approval_mode\":\"unattended\""),
        (&["approval", "set", "local"], "Approval Mode updated to 'local'"),
        (&["approval", "set", "none"], "Approval Mode updated to 'unattended'"),
        (&["sharing", "get"], "Sharing Scope: own-user"),
        (&["sharing", "get", "--json"], "\"sharing_scope\":\"own-user\""),
        (&["sharing", "set", "tailnet"], "Sharing Scope updated to 'tailnet'"),
        (&["sharing", "set", "own-user"], "Sharing Scope updated to 'own-user'"),
    ];
    for &(args, expected) in cases {
        let out = wait_for_child(frd_command(args).spawn().unwrap());
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains(expected));
    }
}

#[test]
fn probe_host_generates_valid_report_envelope() {
    let report = DaemonStatusReport::probe_host(None);
    assert_eq!(report.schema_version, "fr.status.v1");
    assert!(report.timestamp_unix_ms > 0);
    assert!(report.outcome == "success" || report.outcome == "refusal");
    assert!(
        report
            .permissions
            .iter()
            .any(|p| p.capability == "screen_capture")
    );
    assert!(report.capabilities.iter().any(|c| c.name == "video_encode"));
    let json = report.render_json();
    assert!(json.contains("\"schema_version\":\"fr.status.v1\""));
    assert!(json.contains("\"outcome\":"));
    let human = report.render_human();
    assert!(human.contains("FRANKENREMOTE HOST DAEMON STATUS"));
}

#[test]
fn frd_run_cli_absent_tailscale_reports_typed_refusal() {
    let missing = std::env::temp_dir().join(format!("frd-missing-{}.sock", std::process::id()));
    let output = wait_for_child(
        frd_command(&["run", "--socket", missing.to_str().unwrap(), "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    let json_text = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(json_text.contains("\"outcome\":\"refusal\""));
    assert!(json_text.contains("\"code\":\"tailscale_unavailable\""));
}

#[test]
fn frd_headless_cli_option_accepted_in_help_and_run() {
    let help_out = wait_for_child(frd_command(&["--help"]).spawn().unwrap());
    assert!(help_out.status.success());
    let help_text = String::from_utf8_lossy(&help_out.stdout);
    assert!(help_text.contains("--headless"));

    let missing =
        std::env::temp_dir().join(format!("frd-missing-headless-{}.sock", std::process::id()));
    let output = wait_for_child(
        frd_command(&[
            "run",
            "--headless",
            "--socket",
            missing.to_str().unwrap(),
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    let json_text = String::from_utf8_lossy(&output.stdout);
    assert!(json_text.contains("\"code\":\"tailscale_unavailable\""));
}
