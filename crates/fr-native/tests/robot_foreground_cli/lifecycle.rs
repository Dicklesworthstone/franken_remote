//! Actual `fr` foreground owners, not a protocol server fixture. Discovery is
//! deliberately unavailable, so these tests cover local lifecycle and cleanup
//! during retries, never a successful remote session, native decode or grant.
use super::*;

fn target(fixture: &Fixture) -> &str {
    fixture.0.file_name().unwrap().to_str().unwrap()
}
fn start(fixture: &Fixture) -> Process {
    let absent = fixture.0.join("absent.sock");
    Process::spawn(&[
        "robot",
        "session",
        "open",
        target(fixture),
        "--foreground",
        "--role",
        "view",
        "--experimental-native",
        "--display",
        "only",
        "--worker",
        "/usr/bin/false",
        "--x-display",
        ":987",
        "--socket",
        absent.to_str().unwrap(),
        "--attempts",
        "32",
        "--json",
    ])
}
fn status(target: &str) -> Output {
    run(&["robot", "session", "status", target, "--json"])
}
fn identity(output: &Output, target: &str) -> String {
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
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
    assert!(data.get("lease").is_none());
    let id = data["connection_id"].as_str().unwrap();
    assert_eq!(id.len(), 32);
    assert!(
        id.bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    id.to_owned()
}
fn await_identity(owner: &mut Process, target: &str) -> String {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            owner.running(),
            "foreground owner exited before local identification"
        );
        let output = status(target);
        if output.status.success() {
            return identity(&output, target);
        }
        refused(&output, "no_local_session", 2);
        assert!(
            Instant::now() < until,
            "foreground owner never bound its endpoint"
        );
        thread::sleep(Duration::from_millis(10));
    }
}
fn pinned_close(target: &str, id: &str) -> Output {
    run(&[
        "robot",
        "session",
        "close",
        target,
        "--connection-id",
        id,
        "--json",
    ])
}
fn collect(owner: Process, target: &str, id: &str) {
    let closed = pinned_close(target, id);
    assert!(closed.status.success());
    let value: serde_json::Value = serde_json::from_slice(&closed.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["data"]["scope"], "local_connection");
    assert_eq!(value["data"]["target"], target);
    assert_eq!(value["data"]["connection_id"], id);
    assert_eq!(value["data"]["closed"], true);
    assert_eq!(value["data"]["local_cleanup_confirmed"], true);
    // Local closure cancels the actual supervisor and then collects its cleanup.
    // The requesting CLI succeeds; the original foreground CLI reports its stop.
    let ended = owner.finish();
    assert_eq!(ended.status.code(), Some(130));
    let value: serde_json::Value = serde_json::from_slice(&ended.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["outcome"], "cancelled");
    assert_eq!(value["error"]["code"], "cancelled");
    assert!(value.get("lease").is_none());
    assert!(value.get("control_granted").is_none());
    refused(&status(target), "no_local_session", 2);
}

#[test]
fn actual_foreground_owner_is_inspected_and_closed_without_any_remote_session() {
    let fixture = Fixture::new();
    let target = target(&fixture);
    let mut owner = start(&fixture);
    let id = await_identity(&mut owner, target);
    // Keep the owner through the first retry/backoff. Inspection must neither
    // spend a stop request nor replace the invocation with a new one.
    thread::sleep(Duration::from_millis(350));
    assert!(owner.running());
    assert_eq!(identity(&status(target), target), id);
    let wrong = if id == "0".repeat(32) {
        "1".repeat(32)
    } else {
        "0".repeat(32)
    };
    refused(&pinned_close(target, &wrong), "local_session_changed", 1);
    assert!(owner.running());
    assert_eq!(identity(&status(target), target), id);
    collect(owner, target, &id);
}

#[test]
fn replacement_foreground_owner_rejects_old_pins_and_duplicate_invocations() {
    let fixture = Fixture::new();
    let target = target(&fixture);
    let mut first = start(&fixture);
    let old = await_identity(&mut first, target);
    collect(first, target, &old);

    let mut next = start(&fixture);
    let current = await_identity(&mut next, target);
    assert_ne!(
        current, old,
        "new invocation must not reuse its predecessor's pin"
    );
    refused(&pinned_close(target, &old), "local_session_changed", 1);
    assert!(next.running());
    assert_eq!(identity(&status(target), target), current);

    let duplicate = start(&fixture).finish();
    refused(&duplicate, "local_session_already_running", 2);
    assert!(next.running());
    assert_eq!(identity(&status(target), target), current);
    collect(next, target, &current);
}
