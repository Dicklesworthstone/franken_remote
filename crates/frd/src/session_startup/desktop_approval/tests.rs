//! Protocol fixtures do not establish physical consent. They exercise the
//! ORIGINAL Approval, real child ownership, and production parent IPC code.
use super::*;
use crate::session_startup::{self, ALLOWED, DENIED, WAITING};
use asupersync::{cx::Cx, runtime::{Runtime, RuntimeBuilder}, types::{Budget, CancelKind}};
use fr_core::{ids::{HostBootId, OsSessionId, RemoteSessionId}, indicator_process::{Frame, Kind}};
use fr_wire::negotiation::{ControlBinding, Role};
use std::sync::atomic::AtomicU8;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread().enable_platform_reactor(true).build().unwrap()
}
fn original(cx: &Cx, role: Role) -> (Approval, Arc<AtomicU8>) {
    let state = Arc::new(AtomicU8::new(WAITING));
    (Approval {
        binding: ControlBinding {
            id: 7, host_boot: HostBootId::from_raw(11), os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        role, state: Arc::downgrade(&state), cx: cx.clone(),
        deadline: session_startup::now(cx).unwrap() + 2_000_000,
    }, state)
}
fn shared(original: Approval) -> Shared {
    Shared { original, cancelled: AtomicBool::new(false), started: Instant::now() }
}

#[test]
fn native_yes_is_one_use_and_targets_only_its_original_capability() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    for role in [Role::Observe, Role::RequestControl] {
        let (approval, state) = original(&cx, role);
        let owner = shared(approval);
        assert_eq!(consume(&owner, Ok(true)), Ok(true));
        assert_eq!(state.load(Ordering::Acquire), ALLOWED);
        assert!(consume(&owner, Ok(true)).is_err());
    }
}
#[test]
fn local_stop_or_original_cancellation_beats_a_delivered_yes() {
    for local in [false, true] {
        let rt = runtime();
        let cx = rt.request_cx_with_budget(Budget::INFINITE);
        let (approval, state) = original(&cx, Role::Observe);
        let owner = shared(approval);
        if local { owner.cancel(); }
        else { cx.cancel_fast(CancelKind::User); }
        assert!(consume(&owner, Ok(true)).is_err());
        assert_ne!(state.load(Ordering::Acquire), ALLOWED);
        if local { assert_eq!(state.load(Ordering::Acquire), DENIED); }
    }
}
#[test]
fn expiration_and_reused_numeric_identity_never_revive_a_native_yes() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (mut approval, state) = original(&cx, Role::Observe);
    approval.deadline = session_startup::now(&cx).unwrap();
    assert!(consume(&shared(approval), Ok(true)).is_err());
    assert_eq!(state.load(Ordering::Acquire), WAITING);
    let (old, old_state) = original(&cx, Role::Observe);
    drop(old_state);
    let (new, new_state) = original(&cx, Role::Observe);
    assert_eq!(old.binding(), new.binding());
    assert!(consume(&shared(old), Ok(true)).is_err());
    assert_eq!(new_state.load(Ordering::Acquire), WAITING);
}
#[test]
fn native_failure_and_denial_are_not_positive_consent() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (approval, state) = original(&cx, Role::RequestControl);
    let owner = shared(approval);
    assert_eq!(consume(&owner, Err(Error::Protocol)), Err(Error::Protocol));
    assert_eq!(state.load(Ordering::Acquire), WAITING);
    assert_eq!(consume(&owner, Ok(false)), Ok(false));
    assert_eq!(state.load(Ordering::Acquire), DENIED);
}
#[test]
fn exact_role_sequence_and_epoch_are_required_before_interpreting_consent() {
    let request = Frame { kind: Kind::ApprovalCheck, sequence: 8, epoch: 29 };
    for (kind, expected) in [(Kind::Ready, Ok(None)), (Kind::Allowed, Ok(Some(true))),
        (Kind::Denied, Ok(Some(false))), (Kind::Refused, Err(Error::Unavailable))] {
        assert_eq!(io::response(request, &request.reply(kind).encode().unwrap()), expected);
    }
    for reply in [request, request.reply(Kind::Stopped),
        Frame { epoch: 30, ..request.reply(Kind::Allowed) },
        Frame { sequence: 7, ..request.reply(Kind::Allowed) }] {
        assert_eq!(io::response(request, &reply.encode().unwrap()), Err(Error::Protocol));
    }
    let ordinary = Frame { kind: Kind::Check, ..request };
    assert_eq!(io::response(ordinary, &ordinary.reply(Kind::Allowed).encode().unwrap()), Err(Error::Protocol));
}

#[test]
#[allow(clippy::too_many_lines)]
fn real_protocol_children_cannot_bypass_readiness_correlation_or_retirement() {
    use std::os::unix::fs::PermissionsExt;
    const CHILD: &str = r#"#!/usr/bin/python3
import socket, time
mode = '@MODE@'
s = socket.socket(fileno=0)
n = 0
while True:
    data = bytearray()
    while len(data) < 32:
        part = s.recv(32-len(data))
        if not part:
            raise SystemExit(0)
        data.extend(part)
    n += 1
    expected = @ROLE@ if n == 1 else 6
    if data[:4] != b'FRIV' or data[5] != expected:
        raise SystemExit(1)
    if mode == 'exit':
        raise SystemExit(0)
    if mode == 'stall' and n > 1:
        time.sleep(30)
    data[5] = 129 if n == 1 else (133 if mode == 'deny' else 132)
    if mode == 'wrong-sequence':
        data[15] ^= 1
    if mode == 'wrong-epoch':
        data[31] ^= 1
    if mode == 'fragmented':
        for i in range(0, 32, 3):
            s.sendall(data[i:i+3])
    else:
        s.sendall(data)
    if n > 1:
        # A finished reply alone is not cleanup: the real parent must reap us
        # before its original approval becomes consumable.
        time.sleep(30)
"#;
    let rt = runtime();
    for role in [Role::Observe, Role::RequestControl] {
        for mode in ["allow", "deny", "fragmented", "wrong-sequence", "wrong-epoch", "exit", "stall"] {
            let cx = rt.request_cx_with_budget(Budget::INFINITE);
            let (approval, state) = original(&cx, role);
            let path = std::env::temp_dir().join(format!("fr-approval-fixture-{}-{role:?}-{mode}.py", std::process::id()));
            std::fs::write(&path, CHILD.replace("@MODE@", mode)
                .replace("@ROLE@", if role == Role::Observe { "4" } else { "5" })).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            let launch = ProcessLaunch::new(&path, ":0", None, 29).unwrap();
            let mut prompt = Prompt::start(approval, launch).unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            if mode == "allow" {
                loop {
                    if let Some(cleanup) = prompt.try_finish() { assert_eq!(cleanup, Ok(())); break; }
                    assert!(Instant::now() < until);
                    thread::sleep(TURN);
                }
                assert!(OCCUPIED.load(Ordering::Acquire), "unconsumed answer retains capacity");
                assert_eq!(state.load(Ordering::Acquire), WAITING, "native reply alone does not approve");
                let (other, other_state) = original(&cx, role);
                let other_launch = ProcessLaunch::new(&path, ":0", None, 30).unwrap();
                assert!(matches!(Prompt::start(other, other_launch), Err(Error::Busy)));
                assert_eq!(other_state.load(Ordering::Acquire), DENIED);
                assert_eq!(state.load(Ordering::Acquire), WAITING);
            }
            let result = loop {
                match prompt.take_decision() {
                    Ok(None) => assert_eq!(state.load(Ordering::Acquire), WAITING),
                    ready => break ready,
                }
                assert!(Instant::now() < until, "child stuck: {mode}");
                thread::sleep(TURN);
            };
            match mode {
                "allow" | "fragmented" => {
                    assert_eq!(result, Ok(Some(true)));
                    assert_eq!(state.load(Ordering::Acquire), ALLOWED);
                    assert_eq!(prompt.try_finish(), Some(Ok(())), "reaped before positive consumption");
                    assert_eq!(prompt.take_decision(), Err(Error::Consumed));
                }
                "deny" => {
                    assert_eq!(result, Ok(Some(false)));
                    assert_eq!(state.load(Ordering::Acquire), DENIED);
                }
                _ => {
                    assert!(result.is_err(), "invalid response admitted: {mode}");
                    assert_ne!(state.load(Ordering::Acquire), ALLOWED);
                }
            }
            prompt.cancel();
            loop {
                if let Some(result) = prompt.try_finish() { assert_eq!(result, Ok(())); break; }
                assert!(Instant::now() < until, "cleanup stuck: {mode}");
                thread::sleep(TURN);
            }
        }
    }
}
