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
    assert!(cap_names.contains(&"video_encode"));
    assert!(cap_names.contains(&"video_decode"));
    assert!(cap_names.contains(&"screen_capture"));
    assert!(cap_names.contains(&"input_injection"));
    assert!(cap_names.contains(&"clipboard_sync"));
    assert!(cap_names.contains(&"audio_playback"));
    assert!(cap_names.contains(&"audio_microphone"));
    assert!(cap_names.contains(&"file_transfer"));

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
    assert!(restr_codes.contains(&"video_hevc_only"));
    assert!(restr_codes.contains(&"audio_opus_only"));
    assert!(restr_codes.contains(&"full_display_only"));
    assert!(restr_codes.contains(&"tailscale_ingress_only"));
    assert!(restr_codes.contains(&"single_controller_authority"));
    assert!(restr_codes.contains(&"no_zero_rtt_application_data"));

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
    assert!(human_render.contains("Status: [OK - OPERATIONAL]"));
    assert!(human_render.contains("1. TAILSCALE NETWORK STATUS:"));
    assert!(human_render.contains("2. TLS 1.3 CERTIFICATE LIFECYCLE:"));
    assert!(human_render.contains("3. OS PERMISSION STATES:"));
    assert!(human_render.contains("4. HARDWARE & MEDIA CAPABILITY MATRIX:"));
    assert!(human_render.contains("5. ACTIVE SESSIONS & AUTHORITY:"));
    assert!(human_render.contains("6. KNOWN RESTRICTIONS & SCOPE LIMITS:"));
    assert!(human_render.contains("7. STRUCTURED DIAGNOSTIC LOG TRAIL:"));
    assert_or_write_fixture("nominal_operational.txt", &human_render);
}

#[test]
fn permission_revoked_failure_reproduction() {
    let report = DaemonStatusReport::permission_revoked("screen_capture");

    // Exact typed failure codes asserted
    assert_eq!(report.outcome, "refusal");
    assert_eq!(report.refusal_code, Some("permission_missing"));
    assert_eq!(
        report.next_action,
        Some(
            "Grant required permission in OS System Settings under Privacy & Security, then restart service."
        )
    );

    // Permission row marked denied
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

    // Dependent capabilities blocked
    let cap_screen = report
        .capabilities
        .iter()
        .find(|c| c.name == "screen_capture")
        .unwrap();
    assert_eq!(cap_screen.status, CapabilityStatus::Blocked);
    assert!(cap_screen.detail.contains("permission denied by host OS"));

    let cap_encode = report
        .capabilities
        .iter()
        .find(|c| c.name == "video_encode")
        .unwrap();
    assert_eq!(cap_encode.status, CapabilityStatus::Blocked);
    assert!(cap_encode.detail.contains("permission denied by host OS"));

    // Structured log emitted
    let err_log = report
        .structured_logs
        .iter()
        .find(|l| l.event_code == "permission_missing")
        .expect("structured log for permission_missing must be emitted");
    assert_eq!(err_log.level, "error");
    assert_eq!(
        err_log.message,
        "Refusal: screen_capture permission is not granted by the host OS"
    );

    // Rendered output
    let json_render = report.render_json();
    assert!(json_render.contains("\"refusal_code\":\"permission_missing\""));
    assert!(json_render.contains("\"capability\":\"screen_capture\",\"status\":\"denied\""));
    assert!(json_render.contains("\"name\":\"screen_capture\",\"status\":\"blocked\""));
    assert_or_write_fixture("permission_revoked.json", &json_render);

    let human_render = report.render_human();
    assert!(human_render.contains("Status: [REFUSAL - ACTION REQUIRED]"));
    assert!(human_render.contains("Refusal Code: permission_missing"));
    assert!(human_render.contains("[DENIED]"));
    assert!(human_render.contains("[BLOCKED]"));
    assert_or_write_fixture("permission_revoked.txt", &human_render);
}

#[test]
fn certificate_expired_failure_reproduction() {
    let report = DaemonStatusReport::certificate_expired();

    // Exact typed failure codes asserted
    assert_eq!(report.outcome, "refusal");
    assert_eq!(report.refusal_code, Some("certificate_expired"));
    assert_eq!(
        report.next_action,
        Some(
            "Run 'tailscale cert <machine>' to provision a fresh HTTPS certificate, or check Tailscale daemon status."
        )
    );

    // Certificate fields
    assert!(!report.certificate.valid);
    assert_eq!(report.certificate.expiry_countdown_secs, Some(0));
    assert_eq!(
        report.certificate.status_message,
        "Certificate expired 120 seconds ago"
    );

    // All capabilities blocked due to inability to admit TLS ingress
    for c in &report.capabilities {
        assert_eq!(
            c.status,
            CapabilityStatus::Blocked,
            "capability {} must be blocked when certificate is expired",
            c.name
        );
    }

    // Structured log emitted
    let err_log = report
        .structured_logs
        .iter()
        .find(|l| l.event_code == "certificate_expired")
        .expect("structured log for certificate_expired must be emitted");
    assert_eq!(err_log.level, "error");
    assert_eq!(
        err_log.message,
        "Refusal: Host Tailscale certificate has expired; ingress connections refused"
    );

    // Rendered output
    let json_render = report.render_json();
    assert!(json_render.contains("\"refusal_code\":\"certificate_expired\""));
    assert!(json_render.contains("\"valid\":false"));
    assert!(json_render.contains("\"expiry_countdown_secs\":0"));
    assert_or_write_fixture("certificate_expired.json", &json_render);

    let human_render = report.render_human();
    assert!(human_render.contains("Status: [REFUSAL - ACTION REQUIRED]"));
    assert!(human_render.contains("Refusal Code: certificate_expired"));
    assert!(human_render.contains("EXPIRED / INVALID"));
    assert_or_write_fixture("certificate_expired.txt", &human_render);
}

#[test]
fn no_hardware_encoder_failure_reproduction() {
    let report = DaemonStatusReport::no_hardware_encoder();

    // Exact typed failure codes asserted
    assert_eq!(report.outcome, "refusal");
    assert_eq!(report.refusal_code, Some("no_hardware_encoder"));
    assert_eq!(
        report.next_action,
        Some(
            "Install hardware HEVC drivers (e.g. VAAPI / NVENC / QuickSync) or enable hardware acceleration in system BIOS."
        )
    );

    // Encoder capability failed with software fallback forbidden
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

    // Other capabilities remain operational
    let dec = report
        .capabilities
        .iter()
        .find(|c| c.name == "video_decode")
        .unwrap();
    assert_eq!(dec.status, CapabilityStatus::Passed);

    // Structured log emitted
    let err_log = report
        .structured_logs
        .iter()
        .find(|l| l.event_code == "no_hardware_encoder")
        .expect("structured log for no_hardware_encoder must be emitted");
    assert_eq!(err_log.level, "error");
    assert_eq!(
        err_log.message,
        "Refusal: Probed video encoder candidate rejected; hardware HEVC is required by plan §3.3"
    );

    // Rendered output
    let json_render = report.render_json();
    assert!(json_render.contains("\"refusal_code\":\"no_hardware_encoder\""));
    assert!(json_render.contains("\"name\":\"video_encode\",\"status\":\"failed\""));
    assert!(json_render.contains("\"hardware_accelerated\":false"));
    assert_or_write_fixture("no_hardware_encoder.json", &json_render);

    let human_render = report.render_human();
    assert!(human_render.contains("Status: [REFUSAL - ACTION REQUIRED]"));
    assert!(human_render.contains("Refusal Code: no_hardware_encoder"));
    assert!(human_render.contains("[FAILED]"));
    assert!(human_render.contains("[SW]"));
    assert_or_write_fixture("no_hardware_encoder.txt", &human_render);
}

#[test]
fn tailnet_disconnected_failure_reproduction() {
    let report = DaemonStatusReport::tailnet_disconnected();

    // Exact typed failure codes asserted
    assert_eq!(report.outcome, "refusal");
    assert_eq!(report.refusal_code, Some("tailnet_disconnected"));
    assert_eq!(
        report.next_action,
        Some("Connect Tailscale via 'tailscale up' or start the tailscaled daemon.")
    );
    assert!(!report.tailscale.connected);

    // Capabilities blocked
    for c in &report.capabilities {
        assert_eq!(c.status, CapabilityStatus::Blocked);
    }

    // Structured log emitted
    let err_log = report
        .structured_logs
        .iter()
        .find(|l| l.event_code == "tailnet_disconnected")
        .expect("structured log for tailnet_disconnected must be emitted");
    assert_eq!(err_log.level, "error");
    assert_eq!(
        err_log.message,
        "Refusal: Tailscale client is not connected to any tailnet node"
    );

    // Rendered output
    let json_render = report.render_json();
    assert!(json_render.contains("\"refusal_code\":\"tailnet_disconnected\""));
    assert!(json_render.contains("\"connected\":false"));
    assert_or_write_fixture("tailnet_disconnected.json", &json_render);

    let human_render = report.render_human();
    assert!(human_render.contains("Status: [REFUSAL - ACTION REQUIRED]"));
    assert!(human_render.contains("Refusal Code: tailnet_disconnected"));
    assert!(human_render.contains("DISCONNECTED"));
    assert_or_write_fixture("tailnet_disconnected.txt", &human_render);
}

#[test]
fn untested_capability_rule_adherence() {
    // Constitutional invariant §7:
    // "An untested row is passed, failed, blocked, or not tested; an untested row is never 'supported with caveats'."
    assert_eq!(CapabilityStatus::NotTested.as_str(), "not tested");
    assert_eq!(CapabilityStatus::NotTested.badge(), "[NOT TESTED]");

    let mut report = DaemonStatusReport::nominal_operational();
    report.capabilities[0].status = CapabilityStatus::NotTested;
    report.capabilities[0].detail = "Untested codec candidate".into();

    let json = report.render_json();
    assert!(json.contains("\"status\":\"not tested\""));
    assert!(!json.to_lowercase().contains("supported with caveats"));
    assert!(!json.to_lowercase().contains("caveat"));

    let human = report.render_human();
    assert!(human.contains("[NOT TESTED]"));
    assert!(!human.to_lowercase().contains("supported with caveats"));
    assert!(!human.to_lowercase().contains("caveat"));
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

    // 1. Schema envelope
    assert!(json_text.contains("\"schema_version\":\"fr.status.v1\""));
    assert!(json_text.contains("\"outcome\":\"success\""));
    assert!(json_text.contains("\"refusal_code\":null"));
    assert!(json_text.contains("\"next_action\":null"));

    // 2. Tailscale section
    assert!(json_text.contains("\"variant\":\"standalone\""));
    assert!(json_text.contains("\"tailnet\":\"example.ts.net\""));
    assert!(json_text.contains("\"connected\":true"));
    assert!(json_text.contains("\"node_name\":\"workstation-alpha\""));

    // 3. Certificate section
    assert!(json_text.contains("\"certificate_name\":\"workstation-alpha.example.ts.net\""));
    assert!(json_text.contains("\"valid\":true"));
    assert!(json_text.contains("\"expiry_countdown_secs\":7776000"));

    // 4. Permissions (6 granted)
    assert!(json_text.contains("\"permissions\":["));
    assert!(json_text.contains("\"capability\":\"screen_capture\",\"status\":\"granted\""));
    assert!(json_text.contains("\"capability\":\"input_injection\",\"status\":\"granted\""));
    assert!(json_text.contains("\"capability\":\"clipboard_sync\",\"status\":\"granted\""));
    assert!(json_text.contains("\"capability\":\"audio_playback\",\"status\":\"granted\""));
    assert!(json_text.contains("\"capability\":\"audio_microphone\",\"status\":\"granted\""));
    assert!(json_text.contains("\"capability\":\"file_transfer\",\"status\":\"granted\""));

    // 5. Capability matrix (8 passed)
    assert!(json_text.contains("\"capabilities\":["));
    assert!(json_text.contains("\"name\":\"video_encode\",\"status\":\"passed\""));
    assert!(json_text.contains("\"name\":\"video_decode\",\"status\":\"passed\""));
    assert!(json_text.contains("\"hardware_accelerated\":true"));

    // 6. Sharing & Sessions
    assert!(json_text.contains("\"sharing_scope\":\"own-user\""));
    assert!(json_text.contains("\"approval_mode\":\"unattended\""));
    assert!(json_text.contains("\"sessions\":["));
    assert!(json_text.contains("\"role\":\"controller\""));

    // 7. Restrictions
    assert!(json_text.contains("\"restrictions\":["));
    assert!(json_text.contains("\"code\":\"video_hevc_only\""));
    assert!(json_text.contains("\"code\":\"audio_opus_only\""));
    assert!(json_text.contains("\"code\":\"full_display_only\""));
    assert!(json_text.contains("\"code\":\"tailscale_ingress_only\""));
    assert!(json_text.contains("\"code\":\"single_controller_authority\""));
    assert!(json_text.contains("\"code\":\"no_zero_rtt_application_data\""));

    // 8. Structured logs
    assert!(json_text.contains("\"structured_logs\":["));
    assert!(json_text.contains("\"event_code\":\"status_check\""));
}

#[test]
fn frd_status_binary_nominal_human_contract() {
    let output = wait_for_child(frd_command(&["status"]).spawn().unwrap());
    assert!(output.status.success());
    let human_text = String::from_utf8_lossy(&output.stdout).to_string();

    assert!(human_text.contains("FRANKENREMOTE HOST DAEMON STATUS (frd)"));
    assert!(human_text.contains("Status: [OK - OPERATIONAL]"));
    assert!(human_text.contains("1. TAILSCALE NETWORK STATUS:"));
    assert!(human_text.contains("2. TLS 1.3 CERTIFICATE LIFECYCLE:"));
    assert!(human_text.contains("3. OS PERMISSION STATES:"));
    assert!(human_text.contains("4. HARDWARE & MEDIA CAPABILITY MATRIX:"));
    assert!(human_text.contains("5. ACTIVE SESSIONS & AUTHORITY:"));
    assert!(human_text.contains("6. KNOWN RESTRICTIONS & SCOPE LIMITS:"));
    assert!(human_text.contains("7. STRUCTURED DIAGNOSTIC LOG TRAIL:"));

    assert!(human_text.contains("screen_capture     [GRANTED]"));
    assert!(human_text.contains("video_encode       [PASSED]       [HW]"));
    assert!(human_text.contains("video_hevc_only"));
    assert!(human_text.contains("own-user"));
}

#[test]
#[cfg(target_os = "linux")]
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
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory.0)
        .unwrap();
    let path = directory.0.join("policy.json");
    let frd_command = |args: &[&str]| {
        let mut command = frd_command(args);
        command.arg("--config").arg(&path);
        command
    };
    // Approval get / set
    let out = wait_for_child(frd_command(&["approval", "get"]).spawn().unwrap());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Approval Mode: unattended"));

    let out = wait_for_child(frd_command(&["approval", "get", "--json"]).spawn().unwrap());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"approval_mode\":\"unattended\""));

    let out = wait_for_child(frd_command(&["approval", "set", "local"]).spawn().unwrap());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Approval Mode updated to 'local'"));

    let out = wait_for_child(frd_command(&["approval", "set", "none"]).spawn().unwrap());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Approval Mode updated to 'unattended'"));

    // Sharing get / set
    let out = wait_for_child(frd_command(&["sharing", "get"]).spawn().unwrap());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Sharing Scope: own-user"));

    let out = wait_for_child(frd_command(&["sharing", "get", "--json"]).spawn().unwrap());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("\"sharing_scope\":\"own-user\""));

    let out = wait_for_child(frd_command(&["sharing", "set", "tailnet"]).spawn().unwrap());
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Sharing Scope updated to 'tailnet'"));

    let out = wait_for_child(
        frd_command(&["sharing", "set", "own-user"])
            .spawn()
            .unwrap(),
    );
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Sharing Scope updated to 'own-user'"));
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
