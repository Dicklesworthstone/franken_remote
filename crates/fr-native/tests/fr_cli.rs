//! Real CLI processes and credential-checked Unix HTTP, not a running Tailscale
//! installation. Root runs exercise successful discovery; non-root runs must
//! refuse the same socket. No production credential override or live host exists.
#![cfg(all(target_os = "linux", feature = "linux-desktop"))]
#![forbid(unsafe_code)]
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::net::UnixListener,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
fn path(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "fr-cli-{}-{}-{name}",
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
fn wait(mut child: Child) -> Output {
    let until = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("bounded CLI process did not finish");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.len() < 8192, "test fixture output bound");
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}
fn json(output: &Output, assertions: &str) {
    let mut parser = Command::new("python3").args(["-c", &format!("import sys,json\nx=json.load(sys.stdin)\nassert x['schema_version']==1\n{assertions}")])
        .stdin(Stdio::piped()).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn().unwrap();
    parser
        .stdin
        .take()
        .unwrap()
        .write_all(&output.stdout)
        .unwrap();
    assert!(
        parser.wait().unwrap().success(),
        "invalid JSON/output contract"
    );
}
#[test]
fn help_is_usable_without_a_display_daemon_or_credentials() {
    let output = wait(command(&["--help"]).spawn().unwrap());
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(
        help.contains("fr hosts")
            && help.contains("--view-only")
            && help.contains("--experimental-native")
    );
}
#[test]
fn argument_refusals_are_machine_readable_and_never_echo_secrets() {
    for (args, code) in [
        (
            vec!["connect", "n-private", "--json"],
            "control_ui_unavailable",
        ),
        (
            vec!["connect", "n-private", "--view-only", "--json"],
            "native_transport_unqualified",
        ),
        (
            vec!["hosts", "--json", "--token", "PRIVATE-TOKEN"],
            "invalid_arguments",
        ),
        (vec!["hosts", "--json", "--json"], "invalid_arguments"),
    ] {
        let output = wait(command(&args).spawn().unwrap());
        assert_eq!(output.status.code(), Some(2));
        json(
            &output,
            &format!("assert x['outcome']=='refused'\nassert x['error']['code']=='{code}'"),
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(!text.contains("n-private") && !text.contains("PRIVATE-TOKEN"));
    }
}
#[test]
fn unavailable_localapi_is_not_reported_as_an_empty_or_healthy_tailnet() {
    let missing = path("absent.sock");
    let output = wait(
        command(&["hosts", "--json", "--socket", missing.to_str().unwrap()])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    json(
        &output,
        "assert x['outcome']=='refused'\nassert x['error']['code']=='tailscale_unavailable'\nassert 'peers' not in x",
    );
}
#[test]
fn malformed_and_oversized_root_files_refuse_before_network_or_native_setup() {
    for content in [
        b"not a certificate PRIVATE-CERTIFICATE".as_slice(),
        &vec![b'x'; 1_048_577],
    ] {
        let roots = path("roots.pem");
        File::create(&roots).unwrap().write_all(content).unwrap();
        let output = wait(
            command(&[
                "connect",
                "n-private",
                "--view-only",
                "--experimental-native",
                "--worker",
                "/absent/worker",
                "--trust-roots",
                roots.to_str().unwrap(),
                "--display",
                "9",
                "--json",
            ])
            .spawn()
            .unwrap(),
        );
        assert_eq!(output.status.code(), Some(1));
        json(&output, "assert x['error']['code']=='invalid_trust_store'");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("PRIVATE-CERTIFICATE"));
    }
}
#[test]
fn output_write_failure_is_a_nonzero_exit_not_a_successful_command() {
    let output = OpenOptions::new().write(true).open("/dev/full").unwrap();
    let mut command = command(&["--help"]);
    command.stdout(output);
    assert_eq!(wait(command.spawn().unwrap()).status.code(), Some(74));
}
fn fixture() -> String {
    let host_key = format!("nodekey:{}", "1".repeat(64));
    let peer_key = format!("nodekey:{}", "2".repeat(64));
    format!(
        r#"{{"Version":"explicit-fixture","BackendState":"Running","TailscaleIPs":["100.64.0.1"],"CurrentTailnet":{{"Name":"fixture.invalid","MagicDNSSuffix":"fixture.ts.net"}},"Self":{{"ID":"n-host","NodeID":1,"PublicKey":"{host_key}","UserID":7,"TailscaleIPs":["100.64.0.1"],"DNSName":"local.fixture.ts.net.","InNetworkMap":true}},"Peer":{{"{peer_key}":{{"ID":"n-peer","NodeID":2,"PublicKey":"{peer_key}","UserID":7,"TailscaleIPs":["100.64.0.2"],"DNSName":"remote.fixture.ts.net.","InNetworkMap":true}}}}}}"#
    )
}
fn discovery(stall_and_signal: bool) -> (Output, bool) {
    let socket_path = path("api.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();
    listener.set_nonblocking(true).unwrap();
    let cancelled = Arc::new(AtomicBool::new(false));
    let stop = cancelled.clone();
    let (tx, rx) = mpsc::sync_channel(1);
    let server = thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let (mut socket, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                Err(error) => panic!("fixture accept: {error}"),
            };
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while request.len() < 2048 && !request.ends_with(b"\r\n\r\n") {
                if socket.read(&mut byte).unwrap_or(0) != 1 {
                    break;
                }
                request.push(byte[0]);
            }
            if request.is_empty() {
                tx.send(false).unwrap();
                return;
            }
            assert!(request.starts_with(b"GET /localapi/v0/status?peers=true "));
            tx.send(true).unwrap();
            if stall_and_signal {
                // No response: cancellation must drop the pending HTTP socket.
                assert_eq!(socket.read(&mut byte).unwrap(), 0);
            } else {
                let body = fixture();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            return;
        }
    });
    let child = command(&["hosts", "--json", "--socket", socket_path.to_str().unwrap()])
        .spawn()
        .unwrap();
    let requested = rx.recv_timeout(Duration::from_secs(3)).unwrap();
    if requested && stall_and_signal {
        assert!(
            Command::new("kill")
                .args(["-INT", &child.id().to_string()])
                .status()
                .unwrap()
                .success()
        );
    }
    let output = wait(child);
    cancelled.store(true, Ordering::Release);
    server.join().unwrap();
    let is_root = asupersync::net::unix::UnixStream::pair()
        .unwrap()
        .0
        .peer_cred()
        .unwrap()
        .uid
        == 0;
    assert_eq!(
        requested, is_root,
        "production LocalAPI must require root socket credentials"
    );
    (output, is_root)
}
#[test]
fn real_cli_discovery_preserves_kernel_credentials_and_unknown_desktop_status() {
    let (output, is_root) = discovery(false);
    if is_root {
        assert!(output.status.success());
        json(
            &output,
            "assert x['snapshot_only'] is True\nassert len(x['peers'])==1\np=x['peers'][0]\nassert p['node_id']=='n-peer'\nassert p['certificate_name']=='remote.fixture.ts.net'\nassert p['addresses']==['100.64.0.2']\nassert p['desktop_available'] is None and p['access_authorized'] is None",
        );
    } else {
        assert_eq!(output.status.code(), Some(1));
        json(
            &output,
            "assert x['error']['code']=='untrusted_localapi'\nassert 'peers' not in x",
        );
    }
}
#[test]
fn sigint_terminates_a_stalled_lookup_without_claiming_success() {
    let (output, is_root) = discovery(true);
    if is_root {
        assert_eq!(output.status.code(), Some(130));
        json(
            &output,
            "assert x['outcome']=='cancelled'\nassert x['error']['code']=='cancelled'",
        );
    } else {
        assert_eq!(output.status.code(), Some(1));
        json(&output, "assert x['error']['code']=='untrusted_localapi'");
    }
}

#[test]
fn display_inspection_requires_no_local_renderer_and_keeps_identity_failures_explicit() {
    let root = path("inspection-root.pem");
    let key = path("inspection-key.pem");
    let openssl = Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "ec",
            "-pkeyopt",
            "ec_paramgen_curve:P-256",
            "-nodes",
            "-keyout",
            key.to_str().unwrap(),
            "-out",
            root.to_str().unwrap(),
            "-days",
            "1",
            "-subj",
            "/CN=Explicit test root",
        ])
        .output()
        .unwrap();
    assert!(
        openssl.status.success(),
        "{}",
        String::from_utf8_lossy(&openssl.stderr)
    );
    let missing = path("inspection-absent.sock");
    let output = wait(
        command(&[
            "displays",
            "n-peer",
            "--experimental-native",
            "--trust-roots",
            root.to_str().unwrap(),
            "--socket",
            missing.to_str().unwrap(),
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    // No DISPLAY, XAUTHORITY, --worker or --display exists in this process. It
    // reaches the real strict LocalAPI lookup instead of native-window setup.
    assert_eq!(output.status.code(), Some(1));
    json(
        &output,
        "assert x['error']['code']=='tailscale_unavailable'\nassert 'displays' not in x",
    );
}
#[test]
fn display_inspection_argument_and_trust_refusals_do_not_echo_private_material() {
    let root = path("bad-inspection-root.pem");
    File::create(&root)
        .unwrap()
        .write_all(b"PRIVATE-ROOT-MATERIAL")
        .unwrap();
    for (args, expected, exit) in [
        (
            vec!["displays", "n-private", "--json"],
            "native_transport_unqualified",
            2,
        ),
        (
            vec![
                "displays",
                "n-private",
                "--experimental-native",
                "--trust-roots",
                root.to_str().unwrap(),
                "--json",
            ],
            "invalid_trust_store",
            1,
        ),
        (
            vec![
                "displays",
                "n-private",
                "--experimental-native",
                "--trust-roots",
                root.to_str().unwrap(),
                "--display",
                "9",
                "--json",
            ],
            "invalid_arguments",
            2,
        ),
    ] {
        let output = wait(command(&args).spawn().unwrap());
        assert_eq!(output.status.code(), Some(exit));
        json(
            &output,
            &format!("assert x['error']['code']=='{expected}'\nassert x['outcome']=='refused'"),
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(!text.contains("PRIVATE-ROOT") && !text.contains("n-private"));
    }
}

#[test]
fn display_picker_command_keeps_trust_and_view_only_checks_before_native_work() {
    let help = wait(command(&["--help"]).spawn().unwrap());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("--display HANDLE|only|choose")
    );
    let missing = path("private-roots.pem");
    let output = wait(
        command(&[
            "connect",
            "n-private",
            "--view-only",
            "--experimental-native",
            "--worker",
            "/private/worker",
            "--trust-roots",
            missing.to_str().unwrap(),
            "--display",
            "choose",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    json(&output, "assert x['error']['code']=='invalid_trust_store'");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("n-private"));
    let output = wait(
        command(&["connect", "n-private", "--display", "choose", "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(2));
    json(
        &output,
        "assert x['error']['code']=='control_ui_unavailable'",
    );
}

#[test]
fn doctor_cli_checks_help_and_refuses_invalid_arguments() {
    let help = wait(command(&["--help"]).spawn().unwrap());
    let help_text = String::from_utf8(help.stdout).unwrap();
    assert!(help_text.contains("fr doctor"));
    assert!(help_text.contains(
        "Doctor diagnoses installed Tailscale status, service port collisions, and certificate lifecycle."
    ));

    // Port 0 refused
    let output = wait(
        command(&["doctor", "--port", "0", "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(2));
    json(&output, "assert x['error']['code']=='invalid_arguments'");

    // Unrelated flags refused
    let output = wait(
        command(&["doctor", "--view-only", "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(2));
    json(&output, "assert x['error']['code']=='invalid_arguments'");

    // Positional arguments refused
    let output = wait(command(&["doctor", "extra", "--json"]).spawn().unwrap());
    assert_eq!(output.status.code(), Some(2));
    json(&output, "assert x['error']['code']=='invalid_arguments'");

    // Absent socket reported as tailscale_unavailable
    let missing = path("doctor-absent.sock");
    let output = wait(
        command(&["doctor", "--socket", missing.to_str().unwrap(), "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    json(
        &output,
        "assert x['error']['code']=='tailscale_unavailable'",
    );
}

#[test]
fn status_and_disconnect_cli_commands_share_staged_envelope() {
    // Status with missing socket refuses honestly
    let missing = path("status-absent.sock");
    let output = wait(
        command(&["status", "--socket", missing.to_str().unwrap(), "--json"])
            .spawn()
            .unwrap(),
    );
    assert_eq!(output.status.code(), Some(1));
    json(
        &output,
        "assert x['outcome']=='refused'\nassert x['error']['code']=='tailscale_unavailable'",
    );

    // Disconnect succeeds with closed confirmed
    let output = wait(
        command(&["disconnect", "host-beta", "--json"])
            .spawn()
            .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='success'\nassert x['data']['closed'] is True\nassert x['data']['host']=='host-beta'\nassert x['data']['cleanup_confirmed'] is True",
    );

    // Human-readable rendering for disconnect
    let output = wait(command(&["disconnect", "host-beta"]).spawn().unwrap());
    assert!(output.status.success());
    let human = String::from_utf8(output.stdout).unwrap();
    assert!(human.contains("Session Closed: session-host-beta"));
    assert!(human.contains("Cleanup Confirmed: true"));
}

#[test]
fn robot_cli_subcommands_drive_full_scripted_workflow() {
    // 1. Session Open
    let output = wait(
        command(&[
            "robot",
            "session",
            "open",
            "workstation-1",
            "--role",
            "control",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='success'\nassert x['stage']=='admitted'\nassert x['data']['role']=='control'\nassert x['data']['lease_handle'].startswith('lease-local-')",
    );

    // Human-readable session open
    let output = wait(
        command(&[
            "robot",
            "session",
            "open",
            "workstation-1",
            "--role",
            "control",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    let human = String::from_utf8(output.stdout).unwrap();
    assert!(human.contains("Session Open: session-workstation-1"));
    assert!(human.contains("Role: control"));
    assert!(human.contains("Lease Handle: lease-local-"));

    // 2. Observe
    let output = wait(
        command(&[
            "robot",
            "observe",
            "workstation-1",
            "--display",
            "1",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='success'\nassert x['stage']=='observed'\nassert x['data']['geometry']['display_index']==1\nassert x['data']['geometry_generation']==1",
    );

    // 3. Input - normal execution
    let output = wait(
        command(&[
            "robot",
            "input",
            "workstation-1",
            "--lease",
            "lease-local-abc123",
            "--request-id",
            "req-001",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='success'\nassert x['stage']=='submitted_to_os'\nassert x['data']['request_id']=='req-001'\nassert x['data']['disposition']=='committed'",
    );

    // 4. Input - stale geometry precondition refusal
    let output = wait(
        command(&[
            "robot",
            "input",
            "workstation-1",
            "--lease",
            "lease-local-abc123",
            "--request-id",
            "req-002",
            "--precondition-geometry",
            "99",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='refusal'\nassert x['error']['code']=='geometry_generation_stale'",
    );

    // 5. Input - expired observation precondition refusal
    let output = wait(
        command(&[
            "robot",
            "input",
            "workstation-1",
            "--lease",
            "lease-local-abc123",
            "--request-id",
            "req-003",
            "--max-observation-age",
            "0",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='refusal'\nassert x['error']['code']=='observation_expired'",
    );

    // 6. Session Close
    let output = wait(
        command(&[
            "robot",
            "session",
            "close",
            "workstation-1",
            "--lease",
            "lease-local-abc123",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='success'\nassert x['stage']=='observed'\nassert x['data']['closed'] is True\nassert x['data']['cleanup_confirmed'] is True",
    );
}

#[test]
fn robot_cli_artifacts_and_advanced_preconditions() {
    let temp_screen = std::env::temp_dir().join("fr_test_screenshot_art.png");
    let screen_str = temp_screen.to_string_lossy().to_string();

    // 1. Observe with screenshot artifact capture
    let output = wait(
        command(&[
            "robot",
            "observe",
            "workstation-1",
            "--screenshot",
            &screen_str,
            "--evidence-level",
            "submitted_to_compositor",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='success'\nassert x['stage']=='observed'\nassert x['data']['artifact'] is not None\nassert x['data']['artifact']['evidence_level']=='submitted_to_compositor'\nassert len(x['data']['artifact']['sha256'])==64",
    );

    // 2. Precondition lease mismatch refusal
    let output = wait(
        command(&[
            "robot",
            "input",
            "workstation-1",
            "--lease",
            "lease-local-abc123",
            "--request-id",
            "req-mismatch-lease",
            "--precondition-lease",
            "lease-different-999",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='refusal'\nassert x['error']['code']=='lease_mismatch_or_expired'",
    );

    // 3. Precondition focus mismatch refusal (best-effort)
    let output = wait(
        command(&[
            "robot",
            "input",
            "workstation-1",
            "--lease",
            "lease-local-abc123",
            "--request-id",
            "req-focus-mismatch",
            "--precondition-focus",
            "mismatched-window",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='refusal'\nassert x['error']['code']=='focus_mismatch'",
    );

    // 4. Semantic evidence confirmed via adapter
    let output = wait(
        command(&[
            "robot",
            "input",
            "workstation-1",
            "--lease",
            "lease-local-abc123",
            "--request-id",
            "req-semantic-verified",
            "--semantic-evidence",
            "adapter",
            "--json",
        ])
        .spawn()
        .unwrap(),
    );
    assert!(output.status.success());
    json(
        &output,
        "assert x['outcome']=='success'\nassert x['stage']=='observed'\nassert x['data']['observed_application_result']['confirmed'] is True\nassert x['data']['observed_application_result']['evidence_type']=='semantic_adapter'",
    );
}
