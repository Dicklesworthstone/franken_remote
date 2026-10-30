//! Explicit robot requests traverse the shipped foreground owner, its original
//! UDP/TLS control lease and the real per-lease XTest executor. Independent Xlib
//! clients observe effects on the host, never counters substituted for input.
//! LocalAPI, CA and ingress are the existing namespace fixtures; this is not
//! live-tailnet, physical-device or hardware-codec qualification.
use super::*;
use std::{ffi::OsStr, process::Output};

const BLUR: &str = r#"
def event(e):
    pass
def command(cmd):
    assert cmd == ["blur"]
    x.XSetInputFocus(d, root, 2, 0)
    x.XSync(d, 0)
    say("OK")
say("READY")
serve(event, command)
"#;

fn start_automation() -> Controlled {
    let worker = sibling("fr-media-worker");
    let host = Xvfb::start("640x480x24");
    let viewer = Xvfb::start("1600x900x24");
    let observer = Harness::start(HOST_OBSERVER, &host.display, "READY");
    let driver = Harness::start(VIEWER_DRIVER, &viewer.display, "WATCHING");
    let daemon = Daemon::start(
        &host.display,
        &worker,
        Some(sibling("fr-input-agent")),
        false,
        None,
    );
    let client_api = ClientApi::new();
    let client = Command::new(sibling("fr"))
        .args([
            "robot",
            "session",
            "open",
            "n-host",
            "--foreground",
            "--role",
            "control",
            "--accept-input",
            "--experimental-native",
            "--display",
            "only",
            "--json",
        ])
        .arg("--socket")
        .arg(&client_api.path)
        .arg("--trust-roots")
        .arg(fixture::pki().join("ca.pem"))
        .arg("--port")
        .arg(address().port().to_string())
        .args(["--x-display", &viewer.display])
        .arg("--worker")
        .arg(worker)
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut session = Controlled {
        observer,
        driver,
        daemon,
        client,
        window: (0, (0, 0)),
        _client_api: client_api,
        viewer,
        host,
    };
    session.window = session.viewer_window();
    // Do not use await_control(): this owner deliberately does not forward
    // human pointer motion. Only its authenticated status may report readiness.
    session
}

fn invoke<I, S>(args: I) -> (Output, serde_json::Value)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let child = Command::new(sibling("fr"))
        .args(args)
        .arg("--json")
        .env_remove("DISPLAY")
        .env_remove("XAUTHORITY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = wait_for(child, Duration::from_secs(10));
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "robot command did not return JSON: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output, value)
}

fn ready(session: &mut Controlled, connection: &str) -> serde_json::Value {
    let mut snapshot = None;
    assert!(
        eventually(Duration::from_secs(60), || {
            session.alive();
            let (output, reply) = invoke([
                "robot",
                "input",
                "n-host",
                "--connection-id",
                connection,
                "--status",
            ]);
            assert!(output.status.success(), "{reply}");
            assert_eq!(reply["outcome"], "success", "{reply}");
            let data = &reply["data"];
            if data["ready"] == true
                && data["source_age_upper_ms"]
                    .as_u64()
                    .is_some_and(|age| age < 500)
            {
                for field in ["observation_id", "next_request_id"] {
                    assert!(data[field].as_u64().is_some_and(|n| n > 0), "{reply}");
                }
                // Initial protocol generations and the first frame may be zero;
                // their presence, not a fabricated nonzero floor, is required.
                for field in [
                    "geometry_generation",
                    "viewport_generation",
                    "configuration_generation",
                    "recovery_generation",
                    "frame",
                ] {
                    assert!(data[field].as_u64().is_some(), "{reply}");
                }
                assert_eq!(data["physical_visibility_proven"], false, "{reply}");
                snapshot = Some(data.clone());
                true
            } else {
                false
            }
        }),
        "the original robot owner never became input-ready: {}",
        session.daemon.dump()
    );
    assert!(indicator(&mut session.observer).is_some());
    snapshot.unwrap()
}

fn action(connection: &str, snapshot: &serde_json::Value, flag: &str, value: &str) -> Vec<String> {
    [
        "robot".into(),
        "input".into(),
        "n-host".into(),
        "--connection-id".into(),
        connection.into(),
        "--lease".into(),
        snapshot["lease"].as_str().unwrap().into(),
        "--request-id".into(),
        snapshot["next_request_id"].as_u64().unwrap().to_string(),
        "--observation-id".into(),
        snapshot["observation_id"].as_u64().unwrap().to_string(),
        "--max-observation-age-ms".into(),
        "1000".into(),
        flag.into(),
        value.into(),
    ]
    .into()
}

fn submitted(reply: &serde_json::Value, id: u64, actions: u8, operations: u32) {
    assert_eq!(reply["outcome"], "success", "{reply}");
    assert_eq!(reply["stage"], "submitted_to_os", "{reply}");
    let data = &reply["data"];
    assert_eq!(data["request_id"], id, "{reply}");
    for field in [
        "actions_total",
        "actions_encoded",
        "actions_submitted",
        "acknowledged_actions",
    ] {
        assert_eq!(data[field], actions, "{reply}");
    }
    assert_eq!(data["unacknowledged_actions"], 0, "{reply}");
    assert_eq!(data["submitted_operations"], operations, "{reply}");
    assert_eq!(data["unknown_next_operation"], false, "{reply}");
    assert_eq!(data["host_outcome"], "submitted_to_os", "{reply}");
}

fn effects(observer: &mut Harness, before: usize, expected: &[(u32, u32, Point, bool)]) {
    assert!(
        eventually(Duration::from_secs(10), || {
            &host_events(observer)[before..] == expected
        }),
        "real host events differed: {:?}",
        &host_events(observer)[before..]
    );
}

fn exercise_actions(session: &mut Controlled, connection: &str) -> Vec<String> {
    // Human input remains local even while this explicit automation owner has
    // a real lease. A hidden second action sequencer would break these checks.
    ready(session, connection);
    let resting = host_pointer(&mut session.observer);
    let before = host_events(&mut session.observer).len();
    session.viewer_move((450, 340));
    session.viewer("button 1 1");
    session.viewer("button 1 0");
    session.viewer("tap 0x62");
    thread::sleep(Duration::from_millis(300));
    assert_eq!(host_pointer(&mut session.observer), resting);
    assert_eq!(host_events(&mut session.observer).len(), before);

    let first = ready(session, connection);
    let id = first["next_request_id"].as_u64().unwrap();
    let click = action(connection, &first, "--click", "1,320,300");
    let (output, original) = invoke(&click);
    assert!(output.status.success(), "{original}");
    submitted(&original, id, 2, 4);
    effects(
        &mut session.observer,
        before,
        &[(4, 1, (320, 300), false), (5, 1, (320, 300), false)],
    );
    assert_eq!(host_pointer(&mut session.observer), ((320, 300), 0));

    let keycode: u32 = session.observer.ask("keycode 0x61")[1].parse().unwrap();
    let key = ready(session, connection);
    assert_eq!(key["lease"], first["lease"]);
    assert_eq!(key["next_request_id"], id + 1);
    let before = host_events(&mut session.observer).len();
    let (output, report) = invoke(action(connection, &key, "--key", "4"));
    assert!(output.status.success(), "{report}");
    submitted(&report, id + 1, 2, 2);
    effects(
        &mut session.observer,
        before,
        &[
            (2, keycode, (320, 300), false),
            (3, keycode, (320, 300), false),
        ],
    );

    let scroll = ready(session, connection);
    assert_eq!(scroll["lease"], first["lease"]);
    assert_eq!(scroll["next_request_id"], id + 2);
    let before = host_events(&mut session.observer).len();
    let (output, report) = invoke(action(connection, &scroll, "--scroll", "350,320,0,-1"));
    assert!(output.status.success(), "{report}");
    submitted(&report, id + 2, 1, 3);
    effects(
        &mut session.observer,
        before,
        &[(4, 4, (350, 320), false), (5, 4, (350, 320), false)],
    );

    // The original observation has expired and newer pins have replaced it.
    // An exact duplicate must still return its original result, never re-encode.
    thread::sleep(Duration::from_millis(1100));
    let before = host_events(&mut session.observer).len();
    let (output, duplicate) = invoke(&click);
    assert!(output.status.success(), "{duplicate}");
    assert_eq!(duplicate["data"], original["data"]);
    let mut changed = click.clone();
    *changed.last_mut().unwrap() = "1,400,300".into();
    let (output, refused) = invoke(&changed);
    assert!(!output.status.success(), "{refused}");
    assert_eq!(
        refused["error"]["code"], "robot_request_changed",
        "{refused}"
    );
    thread::sleep(Duration::from_millis(300));
    assert_eq!(host_events(&mut session.observer).len(), before);
    assert_eq!(host_pointer(&mut session.observer), ((350, 320), 0));
    let next = ready(session, connection);
    assert_eq!(next["lease"], first["lease"]);
    assert_eq!(next["next_request_id"], id + 3);
    click
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; two Xvfb displays; real fr and input agent"]
fn foreground_robot_actions_have_host_receipts_and_do_not_replay_after_focus_loss() {
    let mut session = start_automation();
    let (output, identified) = invoke(["robot", "session", "status", "n-host"]);
    assert!(output.status.success(), "{identified}");
    let connection = identified["data"]["connection_id"].as_str().unwrap();
    assert_eq!(connection.len(), 32);
    let original_request = exercise_actions(&mut session, connection);
    let before = host_events(&mut session.observer).len();
    let mut focus = Harness::start(BLUR, &session.viewer.display, "READY");
    assert_eq!(focus.ask("blur"), ["OK"]);
    let output = wait_for(session.client, Duration::from_secs(30));
    let stopped: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{stopped}");
    assert_eq!(stopped["error"]["code"], "control_focus_lost", "{stopped}");
    assert!(eventually(Duration::from_secs(15), || {
        indicator(&mut session.observer).is_none()
    }));
    // The departed owner cannot answer or transfer its consumed request into a
    // fresh connection. The lookup itself is local; it cannot acquire a lease.
    let (output, refused) = invoke(&original_request);
    assert!(!output.status.success(), "{refused}");
    assert_eq!(refused["error"]["code"], "no_local_session", "{refused}");
    assert_eq!(host_pointer(&mut session.observer), ((350, 320), 0));
    assert_eq!(host_events(&mut session.observer).len(), before);
    session.daemon.finish();
}
