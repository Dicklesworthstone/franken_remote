//! Production state machine, private-socketpair supervisor and host-run gate.
//! The child is a protocol fixture, NOT native platform or visibility evidence.
use super::*;
use fr_core::indicator_process::{Frame, Kind};
use std::time::Instant;
static CHILDREN: Mutex<()> = Mutex::new(());

mod watcher;

fn opening(now: u64) -> State {
    State {
        status: Status::Opening,
        until: deadline(now, OPEN_BUDGET).unwrap(),
        last: now,
    }
}
fn launch(path: &std::path::Path, epoch: u128) -> ProcessLaunch {
    let display = if cfg!(target_os = "macos") { "" } else { ":0" };
    ProcessLaunch::new(path, display, None, epoch).unwrap()
}
#[test]
fn reply_latency_consumes_the_request_budget_instead_of_restarting_it() {
    let start = 100;
    let mut state = opening(start);
    assert_eq!(state.ready_at(start, Ok(start + 200_000_000)), Ok(()));
    let until = deadline(start, CHECK_BUDGET).unwrap();
    assert_eq!(state.until, until);
    assert_eq!(state.check_at(Ok(until)), Err(Error::Expired));
    assert_eq!(state.status, Status::Stopped(Error::Expired));
}
#[test]
fn an_expired_lease_or_opening_cannot_be_revived_by_a_fresh_reply() {
    let start = 100;
    for mut state in [
        opening(start),
        State {
            status: Status::Ready,
            until: deadline(start, CHECK_BUDGET).unwrap(),
            last: start,
        },
    ] {
        let ended = state.until;
        assert_eq!(state.ready_at(ended, Ok(ended)), Err(Error::Expired));
        assert_eq!(state.status, Status::Stopped(Error::Expired));
        assert_eq!(
            state.ready_at(ended + 1, Ok(ended + 1)),
            Err(Error::Expired)
        );
    }
}
#[test]
fn future_issued_or_already_late_responses_never_report_ready() {
    let start = 100;
    let mut state = opening(start);
    assert_eq!(state.ready_at(start + 1, Ok(start)), Err(Error::Clock));
    let mut state = opening(start);
    assert_eq!(
        state.ready_at(start, Ok(deadline(start, CHECK_BUDGET).unwrap())),
        Err(Error::Expired)
    );
}
#[test]
fn stopped_owner_preserves_first_cause_across_late_progress() {
    let start = 100;
    for error in [
        Error::LocalRevoke,
        Error::Protocol,
        Error::Stopped,
        Error::Unavailable,
    ] {
        let mut state = State {
            status: Status::Stopped(error),
            until: start + 1,
            last: start,
        };
        assert_eq!(state.ready_at(start, Ok(start)), Err(error));
        assert_eq!(
            state.check_at(Ok(deadline(start, OPEN_BUDGET).unwrap())),
            Err(error)
        );
        assert_eq!(state.status, Status::Stopped(error));
    }
}

#[test]
fn suspend_clock_gap_expires_original_ui_evidence_before_any_new_reply() {
    let mut state = opening(1);
    assert_eq!(state.ready_at(2, Ok(3)), Ok(()));
    // Production uses CLOCK_BOOTTIME / Darwin CLOCK_MONOTONIC. Advancing
    // that original clock models suspend without advancing process uptime.
    let after_sleep = 60_000_000_003;
    assert_eq!(state.check_at(Ok(after_sleep)), Err(Error::Expired));
    assert_eq!(
        state.ready_at(after_sleep, Ok(after_sleep + 1)),
        Err(Error::Expired)
    );
    assert_eq!(state.status, Status::Stopped(Error::Expired));
}

#[test]
fn failed_regressed_or_overflowing_clock_is_terminal() {
    for sample in [Err(Error::Clock), Ok(9)] {
        let mut state = opening(10);
        assert_eq!(state.ready_at(10, sample), Err(Error::Clock));
        assert_eq!(state.ready_at(11, Ok(12)), Err(Error::Clock));
        assert_eq!(state.status, Status::Stopped(Error::Clock));
    }
    let mut state = State {
        status: Status::Ready,
        until: u64::MAX,
        last: u64::MAX - 10,
    };
    assert_eq!(
        state.ready_at(u64::MAX - 5, Ok(u64::MAX - 4)),
        Err(Error::Clock)
    );
    assert_eq!(deadline(u64::MAX, TURN), Err(Error::Clock));
}

#[test]
fn delayed_status_cannot_be_overwritten_by_an_older_ready_sample() {
    let mut state = opening(10);
    assert_eq!(state.check_at(Ok(100)), Ok(100));
    assert_eq!(state.ready_at(20, Ok(99)), Err(Error::Clock));
    assert_eq!(state.last, 100);
    assert_eq!(state.ready_at(101, Ok(102)), Err(Error::Clock));
}
#[test]
fn the_supervisor_requires_the_exact_response_not_merely_a_ready_byte() {
    let request = Frame {
        kind: Kind::Check,
        sequence: 8,
        epoch: 29,
    };
    assert_eq!(
        io::response(request, &request.reply(Kind::Ready).encode().unwrap()),
        Ok(())
    );
    for frame in [
        request,
        request.reply(Kind::Stopped),
        Frame {
            sequence: 7,
            ..request.reply(Kind::Ready)
        },
        Frame {
            epoch: 30,
            ..request.reply(Kind::Ready)
        },
    ] {
        assert_eq!(
            io::response(request, &frame.encode().unwrap()),
            Err(Error::Protocol)
        );
    }
    assert_eq!(io::response(request, &[0; 32]), Err(Error::Protocol));
    assert_eq!(
        io::response(request, &request.reply(Kind::Refused).encode().unwrap()),
        Err(Error::Unavailable)
    );
}
#[test]
fn failure_cause_is_available_without_a_lock_before_waking_the_host() {
    let observed = Arc::new(AtomicBool::new(false));
    let reported = observed.clone();
    let shared = Arc::new_cyclic(|weak: &std::sync::Weak<Shared>| {
        let weak = weak.clone();
        Shared {
            on_stop: Box::new(move || {
                let owner = weak.upgrade().unwrap();
                reported.store(
                    owner
                        .state
                        .try_lock()
                        .is_ok_and(|s| s.status == Status::Stopped(Error::Unavailable)),
                    Ordering::Release,
                );
            }),
            state: Mutex::new(opening(now_ns().unwrap())),
            changed: std::sync::Condvar::new(),
            #[cfg(target_os = "linux")]
            watcher_wake: crate::host_indicator::process::watcher::Wake::new().unwrap(),
            native_wake: crate::native_wait::Wake::new().unwrap(),
        }
    });
    shared.stop(Error::Unavailable);
    assert!(observed.load(Ordering::Acquire));
}

#[test]
fn poisoned_ready_evidence_stops_the_original_host_and_preserves_its_cause() {
    let stopped = Arc::new(AtomicBool::new(false));
    let original = stopped.clone();
    let shared = Shared {
        on_stop: Box::new(move || original.store(true, Ordering::Release)),
        state: Mutex::new(opening(now_ns().unwrap())),
        changed: std::sync::Condvar::new(),
        #[cfg(target_os = "linux")]
        watcher_wake: crate::host_indicator::process::watcher::Wake::new().unwrap(),
        native_wake: crate::native_wait::Wake::new().unwrap(),
    };
    let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut state = shared.state.lock().unwrap();
        state.status = Status::Ready;
        panic!("a failed evidence publisher must fence its owner");
    }));
    assert!(poisoned.is_err());
    assert_eq!(shared.status(), Status::Stopped(Error::Panicked));
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(shared.ready(now_ns().unwrap()), Err(Error::Panicked));
    shared.stop(Error::Stopped);
    assert_eq!(shared.status(), Status::Stopped(Error::Panicked));
}

#[test]
fn dropping_a_stopped_owner_retains_its_original_cleanup_worker() {
    // This worker represents an outstanding native teardown. It deliberately
    // cannot complete until the test releases its exact original handle.
    let capacity: &'static Capacity = Box::leak(Box::new(Capacity::new()));
    claim(capacity).unwrap();
    let stopped = Arc::new(AtomicBool::new(false));
    let original = stopped.clone();
    let shared = Arc::new(Shared {
        on_stop: Box::new(move || original.store(true, Ordering::Release)),
        state: Mutex::new(opening(now_ns().unwrap())),
        changed: std::sync::Condvar::new(),
        #[cfg(target_os = "linux")]
        watcher_wake: crate::host_indicator::process::watcher::Wake::new().unwrap(),
        native_wake: crate::native_wait::Wake::new().unwrap(),
    });
    let (release, waiting) = std::sync::mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        waiting.recv().unwrap();
        Ok(())
    });
    let owner = Owner {
        control: Control(shared),
        worker: Some(worker),
        finished: None,
        capacity,
    };
    let control = owner.control();
    drop(owner);
    assert!(stopped.load(Ordering::Acquire));
    assert_eq!(control.status(), Status::Stopped(Error::Stopped));
    assert_eq!(claim(capacity), Err(Error::Busy));
    assert!(capacity.retired.lock().unwrap().is_some());
    release.send(()).unwrap();
    let until = Instant::now() + OPEN_BUDGET;
    while !capacity
        .retired
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .is_finished()
    {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    // Finishing alone does not release the permit; claim joins this retained
    // worker before it can admit a replacement. The old control stays stopped.
    assert!(capacity.occupied.load(Ordering::Acquire));
    assert_eq!(claim(capacity), Ok(()));
    assert!(capacity.retired.lock().unwrap().is_none());
    assert_eq!(control.status(), Status::Stopped(Error::Stopped));
}

#[test]
fn uncertain_retired_cleanup_cannot_admit_a_replacement() {
    let capacity = Capacity::new();
    capacity.occupied.store(true, Ordering::Release);
    let worker = thread::spawn(|| Err(Error::Cleanup));
    let until = Instant::now() + OPEN_BUDGET;
    while !worker.is_finished() {
        assert!(Instant::now() < until);
        thread::yield_now();
    }
    *capacity.retired.lock().unwrap() = Some(worker);
    assert_eq!(claim(&capacity), Err(Error::Cleanup));
    assert_eq!(claim(&capacity), Err(Error::Busy));
    assert!(capacity.occupied.load(Ordering::Acquire));
}

#[test]
#[cfg(host_stack)]
#[allow(clippy::too_many_lines)]
fn real_child_faults_gate_startup_stop_the_original_host_and_drain_before_return() {
    use crate::host_run::StopHandle;
    use fr_core::input_submission::process::{Signal, encode_signal};
    const FIXTURE: &str = r"#!/usr/bin/python3
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
    if mode == 'never-ready':
        time.sleep(30)
    if mode == 'refused':
        data[5] = 131
        s.sendall(data)
        time.sleep(30)
    if mode in ('early-revoke', 'early-revoke-exit', 'early-revoke-refused'):
        socket.socket(fileno=1).send(bytes([@SIGNAL@]))
        if mode == 'early-revoke-refused':
            data[5] = 131
            s.sendall(data)
        if mode != 'early-revoke':
            raise SystemExit(0)
        time.sleep(30)
    if data[5] == 2:
        checks += 1
        if mode == 'stall' or (mode == 'later-stall' and checks > 1):
            time.sleep(30)
        if mode == 'partial-check':
            data[5] = 129
            s.sendall(data[:7])
            time.sleep(30)
        if mode in ('revoke', 'revoke-exit') and checks > 1:
            socket.socket(fileno=1).send(bytes([@SIGNAL@]))
            if mode == 'revoke-exit':
                raise SystemExit(0)
            time.sleep(30)
        if mode == 'failed' and checks > 1:
            socket.socket(fileno=1).send(bytes([@FAILED@]))
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
";
    let _serial = CHILDREN.lock().unwrap();
    let bytes = |signal| {
        encode_signal(signal)
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let (signal, failed) = (bytes(Signal::LocalRevoke), bytes(Signal::IndicatorFailed));
    for mode in [
        "ready",
        "wrong-sequence",
        "wrong-epoch",
        "missing",
        "never-ready",
        "partial-check",
        "refused",
        "exit",
        "stall",
        "fragmented",
        "later-stall",
        "revoke",
        "revoke-exit",
        "failed",
        "early-revoke",
        "early-revoke-exit",
        "early-revoke-refused",
    ] {
        let stop = Arc::new(StopHandle::default());
        let on_stop = stop.clone();
        let path = std::env::temp_dir().join(format!(
            "fr-indicator-fixture-{}-{mode}.py",
            std::process::id()
        ));
        if mode != "missing" {
            crate::executable::write(
                &path,
                FIXTURE
                    .replace("@MODE@", mode)
                    .replace("@SIGNAL@", &signal)
                    .replace("@FAILED@", &failed),
            );
        }
        let mut owner = Owner::start(launch(&path, 29), move || on_stop.request()).unwrap();
        let called = AtomicBool::new(false);
        let drained = AtomicBool::new(false);
        let until = Instant::now() + Duration::from_secs(5);
        let result = super::super::drive(&owner.control(), &stop, || {
            called.store(true, Ordering::Release);
            if matches!(mode, "later-stall" | "revoke" | "revoke-exit" | "failed") {
                while !stop.is_requested() {
                    assert!(
                        Instant::now() < until,
                        "original host was not stopped: {mode}"
                    );
                    thread::sleep(TURN);
                }
                // Simulated host teardown AFTER its original stop, not a
                // cancelled/dropped future. The wrapper must let this complete.
                thread::sleep(TURN);
            }
            drained.store(true, Ordering::Release);
            Ok(())
        });
        let admitted = matches!(
            mode,
            "ready" | "fragmented" | "later-stall" | "revoke" | "revoke-exit" | "failed"
        );
        assert_eq!(
            called.load(Ordering::Acquire),
            admitted,
            "startup gate: {mode}"
        );
        assert_eq!(
            drained.load(Ordering::Acquire),
            admitted,
            "host drain: {mode}"
        );
        assert_eq!(
            result.is_ok(),
            matches!(mode, "ready" | "fragmented" | "revoke" | "revoke-exit")
                || (cfg!(target_os = "macos") && mode.starts_with("early-revoke")),
            "outcome: {mode}: {result:?}"
        );
        // The indicator's own failure stops the host too, but is reported as
        // the indicator being unavailable, never as a local user's revoke.
        if mode == "failed" {
            assert_eq!(
                result,
                Err(super::super::Error::Indicator(Error::Unavailable)),
                "{mode}"
            );
        }
        if matches!(mode, "never-ready" | "partial-check") {
            assert_eq!(
                result,
                Err(super::super::Error::Indicator(Error::Expired)),
                "original request deadline is enforced independently of child progress"
            );
        }
        if matches!(mode, "missing" | "refused") {
            assert_eq!(
                result,
                Err(super::super::Error::Indicator(Error::Unavailable)),
                "a missing native surface never opens the host listener"
            );
        }
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

#[test]
#[allow(clippy::too_many_lines)]
fn terminal_datagrams_survive_child_exit_and_never_relax_framing() {
    use fr_core::input_submission::process::{Signal, encode_signal};
    const FIXTURE: &str = r"#!/usr/bin/python3
import socket
command = socket.socket(fileno=0)
signal = socket.socket(fileno=1)
request = bytearray()
while len(request) < 32:
    part = command.recv(32 - len(request))
    if not part: raise SystemExit(1)
    request.extend(part)
for packet in [@PACKETS@]:
    signal.send(packet)
if '@MODE@' == 'local-refused':
    request[5] = 131
    command.sendall(request)
if '@MODE@' == 'local-partial-reply':
    request[5] = 129
    command.sendall(request[:7])
raise SystemExit(0)
";
    let _serial = CHILDREN.lock().unwrap();
    let local = encode_signal(Signal::LocalRevoke);
    let mut oversized = local.to_vec();
    oversized.push(0);
    let mut malformed = local;
    malformed[15] = 1;
    for (mode, packets, expected) in [
        ("local-eof", vec![local.to_vec()], Error::LocalRevoke),
        ("local-refused", vec![local.to_vec()], Error::LocalRevoke),
        (
            "local-partial-reply",
            vec![local.to_vec()],
            Error::LocalRevoke,
        ),
        ("missing-signal", vec![], Error::Unavailable),
        (
            "native-failure",
            vec![encode_signal(Signal::IndicatorFailed).to_vec()],
            Error::Unavailable,
        ),
        (
            "partial-signal",
            vec![local[..8].to_vec(), local[8..].to_vec()],
            Error::Protocol,
        ),
        ("oversized-signal", vec![oversized], Error::Protocol),
        (
            "malformed-then-local",
            vec![malformed.to_vec(), local.to_vec()],
            Error::Protocol,
        ),
        (
            "wrong-direction",
            vec![encode_signal(Signal::Fence).to_vec()],
            Error::Protocol,
        ),
    ] {
        let path = std::env::temp_dir().join(format!(
            "fr-indicator-terminal-{}-{mode}.py",
            std::process::id()
        ));
        let packets = packets
            .iter()
            .map(|packet| {
                format!(
                    "bytes([{}])",
                    packet
                        .iter()
                        .map(u8::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        crate::executable::write(
            &path,
            FIXTURE
                .replace("@PACKETS@", &packets)
                .replace("@MODE@", mode),
        );
        let stopped = Arc::new(AtomicBool::new(false));
        let original = stopped.clone();
        let mut owner = Owner::start(launch(&path, 31), move || {
            original.store(true, Ordering::Release);
        })
        .unwrap();
        let control = owner.control();
        let until = Instant::now() + OPEN_BUDGET + CHECK_BUDGET;
        let cause = loop {
            if let Status::Stopped(cause) = control.status() {
                break cause;
            }
            assert!(
                Instant::now() < until,
                "original child did not stop: {mode}"
            );
            thread::sleep(TURN);
        };
        assert_eq!(cause, expected, "terminal cause: {mode}");
        assert!(stopped.load(Ordering::Acquire), "original stop: {mode}");
        // EOF or a stopped flag does not collect the child. Its original
        // worker permit stays occupied until this exact owner joins it.
        assert!(OBSERVATION.occupied.load(Ordering::Acquire));
        owner.stop();
        assert_eq!(control.status(), Status::Stopped(expected));
        loop {
            if let Some(result) = owner.try_finish() {
                assert_eq!(result, Ok(()), "original child collection: {mode}");
                break;
            }
            assert!(Instant::now() < until, "cleanup retained: {mode}");
            thread::sleep(TURN);
        }
        assert!(!OBSERVATION.occupied.load(Ordering::Acquire));
        assert_eq!(control.status(), Status::Stopped(expected));
    }
}

#[test]
fn microphone_readiness_is_never_a_positive_approval_reply() {
    for kind in [Kind::OpenMicrophone, Kind::CheckMicrophone] {
        let request = Frame {
            kind,
            sequence: 1,
            epoch: 19,
        };
        assert_eq!(
            io::response(request, &request.reply(Kind::Ready).encode().unwrap()),
            Ok(())
        );
        for reply in [Kind::Allowed, Kind::Denied, Kind::Stopped] {
            assert_eq!(
                io::response(request, &request.reply(reply).encode().unwrap()),
                Err(Error::Protocol)
            );
        }
    }
}

#[test]
fn microphone_child_has_its_own_role_capacity_and_original_stop() {
    const FIXTURE: &str = r"#!/usr/bin/python3
import socket
s = socket.socket(fileno=0)
expected = 1
check = None
while True:
    data = bytearray()
    while len(data) < 32:
        part = s.recv(32 - len(data))
        if not part: raise SystemExit(0)
        data.extend(part)
    if data[:4] != b'FRIV' or int.from_bytes(data[8:16], 'big') != expected:
        raise SystemExit(1)
    if expected == 1:
        if data[5] not in (1, 7): raise SystemExit(1)
        check = 2 if data[5] == 1 else 8
    elif data[5] != check:
        raise SystemExit(1)
    expected += 1
    data[5] = 129
    s.sendall(data)
";
    let _serial = CHILDREN.lock().unwrap();
    let path = std::env::temp_dir().join(format!("fr-indicator-roles-{}.py", std::process::id()));
    crate::executable::write(&path, FIXTURE);
    let observation_stopped = Arc::new(AtomicBool::new(false));
    let microphone_stopped = Arc::new(AtomicBool::new(false));
    let observed = observation_stopped.clone();
    let mut observation = Owner::start(launch(&path, 19), move || {
        observed.store(true, Ordering::Release);
    })
    .unwrap();
    let heard = microphone_stopped.clone();
    let mut microphone = Owner::start_role(
        launch(&path, 20),
        move || heard.store(true, Ordering::Release),
        Role::Microphone,
    )
    .unwrap();
    let until = Instant::now() + OPEN_BUDGET;
    while observation.control().status() != Status::Ready
        || microphone.control().status() != Status::Ready
    {
        assert!(Instant::now() < until);
        assert!(!observation_stopped.load(Ordering::Acquire));
        assert!(!microphone_stopped.load(Ordering::Acquire));
        thread::sleep(TURN);
    }
    let duplicate_stopped = Arc::new(AtomicBool::new(false));
    let duplicate = duplicate_stopped.clone();
    assert!(matches!(
        Owner::start_role(
            launch(&path, 21),
            move || duplicate.store(true, Ordering::Release),
            Role::Microphone,
        ),
        Err(Error::Busy)
    ));
    assert!(duplicate_stopped.load(Ordering::Acquire));
    microphone.stop();
    assert!(microphone_stopped.load(Ordering::Acquire));
    assert!(!observation_stopped.load(Ordering::Acquire));
    assert_eq!(observation.control().status(), Status::Ready);
    for owner in [&mut microphone, &mut observation] {
        owner.stop();
        let until = Instant::now() + OPEN_BUDGET;
        loop {
            if let Some(result) = owner.try_finish() {
                assert_eq!(result, Ok(()));
                break;
            }
            assert!(Instant::now() < until);
            thread::sleep(TURN);
        }
    }
}
