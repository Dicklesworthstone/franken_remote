//! Real processes and kernel-authenticated sockets with an explicit metadata
//! fixture. This is not evidence of a real remote decoder or optical visibility.
#![cfg(all(target_os = "linux", feature = "linux-desktop"))]
#![forbid(unsafe_code)]
use std::{
    io::{self, Read, Write},
    os::{linux::net::SocketAddrExt, unix::net::{SocketAddr, UnixListener}},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};
const INSTANCE: [u8; 16] = [0xa3; 16];
fn target() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("live-observe-cli-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}
enum Reply { Snapshot(Vec<u8>), StalePin, Oversized }
fn fixture(target: &str, replies: Vec<Reply>) -> thread::JoinHandle<()> {
    let (socket, _) = asupersync::net::unix::UnixStream::pair().unwrap();
    let uid = socket.peer_cred().unwrap().uid;
    let digest = fr_client::robot::sha256::sha256_hex(target.as_bytes());
    let address = SocketAddr::from_abstract_name(format!("frankenremote.connect.v1.{uid}.{digest}").as_bytes()).unwrap();
    let listener = UnixListener::bind_addr(&address).unwrap();
    listener.set_nonblocking(true).unwrap();
    let target = target.to_owned();
    thread::spawn(move || {
        for reply in replies {
            let until = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < until, "CLI did not reach local fixture");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut identify = [0; 8];
            stream.read_exact(&mut identify).unwrap();
            assert_eq!(&identify, b"FRSC1ID\0");
            let mut identity = b"FRSC1ME\0".to_vec();
            identity.extend_from_slice(&INSTANCE);
            identity.extend_from_slice(&u16::try_from(target.len()).unwrap().to_be_bytes());
            identity.extend_from_slice(target.as_bytes());
            stream.write_all(&identity).unwrap();
            if matches!(reply, Reply::StalePin) {
                assert_eq!(stream.read(&mut [0]).unwrap(), 0, "stale pin sent a request");
                continue;
            }
            let mut query = [0; 24];
            stream.read_exact(&mut query).unwrap();
            assert_eq!(&query[..8], b"FRSC1OB\0", "read must never send STOP");
            assert_eq!(query[8..], INSTANCE);
            let (size, body) = match reply {
                Reply::Snapshot(body) => (u16::try_from(body.len()).unwrap(), body),
                Reply::Oversized => (2049, Vec::new()),
                Reply::StalePin => unreachable!(),
            };
            let mut response = b"FRSC1OV\0".to_vec();
            response.extend_from_slice(&INSTANCE);
            response.extend_from_slice(&size.to_be_bytes());
            response.extend_from_slice(&body);
            stream.write_all(&response).unwrap();
            assert_eq!(stream.read(&mut [0]).unwrap(), 0, "query sent a follow-up action");
        }
    })
}
fn cli(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_fr")).args(args)
        .env_remove("DISPLAY").env_remove("XAUTHORITY")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= until { let _ = child.kill(); let _ = child.wait(); panic!("local observation stalled"); }
        thread::sleep(Duration::from_millis(5));
    }
    let output = child.wait_with_output().unwrap();
    assert!(output.stdout.len() < 16 * 1024);
    assert!(output.stderr.is_empty());
    output
}
fn query(target: &str, id: &str) -> Output {
    cli(&["robot", "observe", target, "--connection-id", id, "--json"])
}
fn snapshot() -> Vec<u8> {
    use fr_client::robot::live::{Evidence, Observation};
    use fr_core::ids::DisplayGeometryGeneration;
    let mut state = Observation::default();
    state.begin(1).unwrap();
    state.authenticated().unwrap();
    let now = Instant::now();
    state.ready(fr_wire::display::Display {
        handle: 17, geometry: DisplayGeometryGeneration::INITIAL,
        x: 0, y: 0, pixel_width: 800, pixel_height: 600,
        logical_width: 800, logical_height: 600,
        scale_numerator: 1, scale_denominator: 1, rotation: 0,
    }, 0, Evidence::Decoded, now).unwrap();
    serde_json::to_vec(&state.snapshot(now).unwrap()).unwrap()
}
fn json(output: &Output) -> serde_json::Value { serde_json::from_slice(&output.stdout).unwrap() }

#[test]
fn repeated_cli_reads_need_neither_a_display_nor_a_new_remote_connection() {
    let target = target();
    let owner = fixture(&target, vec![Reply::Snapshot(snapshot()), Reply::Snapshot(snapshot())]);
    for _ in 0..2 {
        let output = query(&target, &"a3".repeat(16));
        assert!(output.status.success());
        let value = json(&output);
        assert_eq!(value["outcome"], "success");
        assert_eq!(value["data"]["connection_id"], "a3".repeat(16));
        assert_eq!(value["data"]["new_connection_opened"], false);
        assert_eq!(value["data"]["source_freshness"], "unknown");
        assert_eq!(value["data"]["snapshot"]["frame"]["frame_serial"], 0);
        assert!(value.get("stage").is_none());
    }
    owner.join().unwrap();
}

#[test]
fn stale_pin_and_oversized_reply_refuse_without_any_stop_or_new_connection() {
    for (reply, id, code) in [
        (Reply::StalePin, "b4".repeat(16), "local_session_changed"),
        (Reply::Oversized, "a3".repeat(16), "local_session_protocol"),
    ] {
        let target = target();
        let owner = fixture(&target, vec![reply]);
        let output = query(&target, &id);
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(json(&output)["error"]["code"], code);
        owner.join().unwrap();
    }
}

#[test]
fn unavailable_owner_and_unsupported_privileges_are_never_tailnet_fallbacks() {
    let output = query(&target(), &"a3".repeat(16));
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(json(&output)["error"]["code"], "no_local_session");
    let output = cli(&["robot", "session", "open", "n-host", "--foreground", "--role", "control", "--publish-observations", "--experimental-native", "--display", "only", "--json"]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(json(&output)["error"]["code"], "invalid_arguments");
}
