//! Production state machine, private-socketpair supervisor and host-run gate.
//! The child is a protocol fixture, NOT native X11 or visibility evidence.
use super::*;
use fr_core::indicator_process::{Frame, Kind};

fn opening(now: Instant) -> State {
    State { status: Status::Opening, until: now + OPEN_BUDGET }
}
#[test]
fn reply_latency_consumes_the_request_budget_instead_of_restarting_it() {
    let start = Instant::now();
    let mut state = opening(start);
    assert!(state.ready_at(start, start + Duration::from_millis(200)));
    assert_eq!(state.until, start + CHECK_BUDGET);
    assert_eq!(state.check_at(start + CHECK_BUDGET), Status::Stopped(Error::Expired));
}
#[test]
fn an_expired_lease_or_opening_cannot_be_revived_by_a_fresh_reply() {
    let start = Instant::now();
    for mut state in [opening(start), State { status: Status::Ready, until: start + CHECK_BUDGET }] {
        let ended = state.until;
        assert!(!state.ready_at(ended, ended));
        assert_eq!(state.status, Status::Stopped(Error::Expired));
        assert!(!state.ready_at(ended + TURN, ended + TURN));
    }
}
#[test]
fn future_issued_or_already_late_responses_never_report_ready() {
    let start = Instant::now();
    let mut state = opening(start);
    assert!(!state.ready_at(start + TURN, start));
    let mut state = opening(start);
    assert!(!state.ready_at(start, start + CHECK_BUDGET));
}
#[test]
fn stopped_owner_preserves_first_cause_across_late_progress() {
    let start = Instant::now();
    for error in [Error::LocalRevoke, Error::Protocol, Error::Stopped, Error::Unavailable] {
        let mut state = State { status: Status::Stopped(error), until: start + TURN };
        assert!(!state.ready_at(start, start));
        assert_eq!(state.check_at(start + OPEN_BUDGET), Status::Stopped(error));
    }
}
#[test]
fn the_supervisor_requires_the_exact_response_not_merely_a_ready_byte() {
    let request = Frame { kind: Kind::Check, sequence: 8, epoch: 29 };
    assert_eq!(io::response(request, &request.reply(Kind::Ready).encode().unwrap()), Ok(()));
    for frame in [
        request,
        request.reply(Kind::Stopped),
        Frame { sequence: 7, ..request.reply(Kind::Ready) },
        Frame { epoch: 30, ..request.reply(Kind::Ready) },
    ] {
        assert_eq!(io::response(request, &frame.encode().unwrap()), Err(Error::Protocol));
    }
    assert_eq!(io::response(request, &[0; 32]), Err(Error::Protocol));
    assert_eq!(io::response(request, &request.reply(Kind::Refused).encode().unwrap()), Err(Error::Unavailable));
}

#[test]
fn real_child_faults_gate_startup_stop_the_original_host_and_drain_before_return() {
    use crate::host_run::StopHandle;
    use std::os::unix::fs::PermissionsExt;
    use fr_core::input_submission::process::{encode_signal, Signal};
    const FIXTURE: &str = r#"#!/usr/bin/python3
import socket, time
mode = '@MODE@'
s = socket.socket(fileno=0)
checks = 0
while True:
    data = bytearray()
    while len(data) < 32:
        part = s.recv(32 - len(data))
        if not part:
            raise SystemExit(0)
        data.extend(part)
    if data[:4] != b'FRIV' or data[4] != 1:
        raise SystemExit(1)
    if mode == 'exit':
        raise SystemExit(0)
    if data[5] == 2:
        checks += 1
        if mode == 'stall' or (mode == 'later-stall' and checks > 1):
            time.sleep(30)
        if mode == 'revoke' and checks > 1:
            socket.socket(fileno=1).send(bytes([@SIGNAL@]))
            time.sleep(30)
    data[5] = 129
    if mode == 'wrong-sequence':
        data[15] ^= 1
    if mode == 'wrong-epoch':
        data[31] ^= 1
    if mode == 'fragmented':
        for i in range(0, 32, 3):
            s.sendall(data[i:i+3])
    else:
        s.sendall(data)
"#;
    let signal = encode_signal(Signal::LocalRevoke).iter().map(u8::to_string).collect::<Vec<_>>().join(",");
    for mode in ["ready", "wrong-sequence", "wrong-epoch", "exit", "stall", "fragmented", "later-stall", "revoke"] {
        let stop = Arc::new(StopHandle::default());
        let on_stop = stop.clone();
        let path = std::env::temp_dir().join(format!("fr-indicator-fixture-{}-{mode}.py", std::process::id()));
        std::fs::write(&path, FIXTURE.replace("@MODE@", mode).replace("@SIGNAL@", &signal)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let launch = ProcessLaunch::new(&path, ":0", None, 29).unwrap();
        let mut owner = Owner::start(launch, move || on_stop.request()).unwrap();
        let called = AtomicBool::new(false);
        let drained = AtomicBool::new(false);
        let until = Instant::now() + Duration::from_secs(5);
        let result = super::super::drive(&owner, &stop, || {
            called.store(true, Ordering::Release);
            if matches!(mode, "later-stall" | "revoke") {
                while !stop.is_requested() {
                    assert!(Instant::now() < until, "original host was not stopped: {mode}");
                    thread::sleep(TURN);
                }
                // Simulated host teardown AFTER its original stop, not a
                // cancelled/dropped future. The production wrapper must join it.
                thread::sleep(TURN);
            }
            drained.store(true, Ordering::Release);
            Ok(())
        });
        let admitted = matches!(mode, "ready" | "fragmented" | "later-stall" | "revoke");
        assert_eq!(called.load(Ordering::Acquire), admitted, "startup gate: {mode}");
        assert_eq!(drained.load(Ordering::Acquire), admitted, "host drain: {mode}");
        assert_eq!(result.is_ok(), matches!(mode, "ready" | "fragmented" | "revoke"), "outcome: {mode}: {result:?}");
        owner.stop();
        assert!(stop.is_requested(), "stop must precede cleanup: {mode}");
        loop {
            if let Some(result) = owner.try_finish() {
                assert_eq!(result, Ok(()), "original child must be reaped: {mode}");
                break;
            }
            assert!(Instant::now() < until, "cleanup retained: {mode}");
            thread::sleep(TURN);
        }
        assert_eq!(owner.try_finish(), Some(Ok(())));
        // Every following mode must reclaim the SAME global capacity only
        // after the previous thread joined and its child was reaped.
    }
}
