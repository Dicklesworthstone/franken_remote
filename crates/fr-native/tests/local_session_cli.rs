//! Real CLI processes and kernel-authenticated abstract sockets. The server is
//! an explicit local protocol fixture, not a remote desktop or cleanup proof.
#![cfg(all(target_os = "linux", feature = "linux-desktop"))]
#![forbid(unsafe_code)]

use std::{
    io::{self, Read, Write},
    os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixListener, UnixStream},
    },
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const INSTANCE: [u8; 16] = [0xa3; 16];

enum Exchange {
    Inspect,
    Close,
    LoseReply,
}

fn target() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "local-session-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn fixture(target: &str, exchanges: Vec<Exchange>) -> JoinHandle<()> {
    let (socket, _) = asupersync::net::unix::UnixStream::pair().unwrap();
    let uid = socket.peer_cred().unwrap().uid;
    let digest = fr_client::robot::sha256::sha256_hex(target.as_bytes());
    let address = SocketAddr::from_abstract_name(
        format!("frankenremote.connect.v1.{uid}.{digest}").as_bytes(),
    )
    .unwrap();
    let listener = UnixListener::bind_addr(&address).unwrap();
    listener.set_nonblocking(true).unwrap();
    let target = target.to_owned();
    thread::spawn(move || {
        for exchange in exchanges {
            let mut stream = accept(&listener);
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut identify = [0; 8];
            stream.read_exact(&mut identify).unwrap();
            assert_eq!(&identify, b"FRSC1ID\0");
            let mut identity = b"FRSC1ME\0".to_vec();
            identity.extend_from_slice(&INSTANCE);
            identity.extend_from_slice(&u16::try_from(target.len()).unwrap().to_be_bytes());
            identity.extend_from_slice(target.as_bytes());
            stream.write_all(&identity).unwrap();
            match exchange {
                Exchange::Inspect => {
                    let mut byte = [0; 1];
                    assert_eq!(
                        stream.read(&mut byte).unwrap(),
                        0,
                        "no STOP may follow inspection or a stale pin"
                    );
                }
                Exchange::Close | Exchange::LoseReply => {
                    let mut stop = [0; 24];
                    stream.read_exact(&mut stop).unwrap();
                    assert_eq!(&stop[..8], b"FRSC1ST\0");
                    assert_eq!(stop[8..], INSTANCE);
                    if matches!(exchange, Exchange::Close) {
                        let mut reply = b"FRSC1FN\0".to_vec();
                        reply.extend_from_slice(&INSTANCE);
                        reply.push(1);
                        stream.write_all(&reply).unwrap();
                    }
                }
            }
        }
    })
}

fn accept(listener: &UnixListener) -> UnixStream {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return stream,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < until,
                    "CLI never reached its local endpoint"
                );
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("fixture accept: {error}"),
        }
    }
}

fn cli(args: &[&str]) -> Output {
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
            panic!("bounded local CLI call did not finish");
        }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.len() < 16 * 1024);
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {:?}",
        output.stderr
    );
    output
}
fn json(output: &Output) -> serde_json::Value {
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    value
}

#[test]
fn separate_commands_inspect_and_close_the_same_pinned_invocation() {
    let target = target();
    let owner = fixture(&target, vec![Exchange::Inspect, Exchange::Close]);
    let status = cli(&["robot", "session", "status", &target, "--json"]);
    assert!(status.status.success());
    let value = json(&status);
    let data = &value["data"];
    assert_eq!(data["scope"], "local_connection");
    assert_eq!(data["target"], target);
    assert_eq!(data["local_owner_present"], true);
    assert_eq!(
        data.get("remote_session_state"),
        Some(&serde_json::Value::Null)
    );
    assert_eq!(
        data.get("remote_control_granted"),
        Some(&serde_json::Value::Null)
    );
    let id = data["connection_id"].as_str().unwrap();
    assert_eq!(id, "a3".repeat(16));
    let closed = cli(&[
        "robot",
        "session",
        "close",
        &target,
        "--connection-id",
        id,
        "--socket",
        "/absent/not-contacted-tailscaled.sock",
        "--json",
    ]);
    assert!(closed.status.success());
    let value = json(&closed);
    assert_eq!(value["data"]["connection_id"], id);
    assert_eq!(value["data"]["closed"], true);
    assert_eq!(value["data"]["local_cleanup_confirmed"], true);
    owner.join().unwrap();
}

#[test]
fn stale_cli_pin_refuses_without_sending_stop_or_claiming_unknown_effects() {
    let target = target();
    let owner = fixture(&target, vec![Exchange::Inspect]);
    let output = cli(&[
        "robot",
        "session",
        "close",
        &target,
        "--connection-id",
        &"b4".repeat(16),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["error"]["code"], "local_session_changed");
    assert_eq!(value["outcome"], "refused");
    assert!(value.get("data").is_none());
    owner.join().unwrap();
}

#[test]
fn lost_cleanup_reply_after_pinned_stop_is_unknown_not_refused() {
    let target = target();
    let owner = fixture(&target, vec![Exchange::LoseReply]);
    let output = cli(&[
        "robot",
        "session",
        "close",
        &target,
        "--connection-id",
        &"a3".repeat(16),
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(1));
    let value = json(&output);
    assert_eq!(value["error"]["code"], "disconnect_outcome_unknown");
    assert_eq!(value["outcome"], "unknown_external_effect");
    assert!(value.get("data").is_none());
    owner.join().unwrap();
}

#[test]
fn absent_local_owner_is_not_reported_as_a_tailnet_or_display_failure() {
    let target = target();
    let output = cli(&["robot", "session", "status", &target, "--json"]);
    assert_eq!(output.status.code(), Some(2));
    let value = json(&output);
    assert_eq!(value["error"]["code"], "no_local_session");
    assert_eq!(value["outcome"], "refused");
    assert!(value.get("data").is_none());
}

#[test]
fn a_connection_pin_never_enables_an_unissued_robot_lease() {
    let target = target();
    let output = cli(&[
        "robot",
        "session",
        "close",
        &target,
        "--connection-id",
        &"a3".repeat(16),
        "--lease",
        "private-unissued-lease",
        "--json",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "local_lease_unavailable");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-unissued-lease"));
}

#[test]
fn help_explains_local_identity_and_conditional_shutdown() {
    let output = cli(&["--help"]);
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("fr robot session status NODE_ID"));
    assert!(help.contains("--connection-id ID"));
    assert!(help.contains("not a remote session or input lease"));
}
