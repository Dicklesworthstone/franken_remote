//! Original monitor evidence under injected clock values. These exercise the
//! production publication/expiry logic, not native desktop lifecycle events.
use super::*;
use std::time::Duration;

const START: u64 = 1_000_000_000;

fn shared(status: Status, last_ns: u64, until_ns: u64) -> Shared {
    Shared {
        evidence: Mutex::new(Evidence {
            status,
            last_ns,
            until_ns,
        }),
        waker: Mutex::new(None),
    }
}

fn active(until_ns: u64) -> protocol::Reply {
    protocol::Reply {
        state: State::Active,
        until_ns,
    }
}

fn assert_terminal(shared: &Shared, error: Error) {
    // A terminal observation does not even read a later clock or permit a
    // responsive child to replace its cause with fresh-looking evidence.
    assert_eq!(
        shared.status_with(|| panic!("terminal evidence read a new clock")),
        Status::Stopped(error)
    );
    assert_eq!(
        shared.accept_with(active(u64::MAX), START, || {
            panic!("terminal evidence accepted a new response")
        }),
        Err(error)
    );
    shared.stop(Error::Stopped);
    assert_eq!(shared.evidence().status, Status::Stopped(error));
}

#[test]
fn reply_cannot_refresh_the_previous_opening_or_active_deadline() {
    for (status, error) in [
        (Status::Opening, Error::OpeningExpired),
        (Status::Active, Error::EvidenceExpired),
    ] {
        let shared = shared(status, START, START + 100);
        assert_eq!(
            shared.accept_with(active(START + VALID_NS), START + 90, || Ok(START + 100)),
            Err(error)
        );
        assert_terminal(&shared, error);
    }
}

#[test]
fn active_evidence_keeps_both_the_source_deadline_and_original_request_cap() {
    for source_until in [START + 100, START + VALID_NS * 2] {
        let shared = shared(Status::Opening, START, START + OPEN_NS);
        assert_eq!(
            shared.accept_with(active(source_until), START, || Ok(START + 10)),
            Ok(())
        );
        let until = source_until.min(START + VALID_NS);
        assert_eq!(shared.status_with(|| Ok(until - 1)), Status::Active);
        assert_eq!(
            shared.status_with(|| Ok(until)),
            Status::Stopped(Error::EvidenceExpired)
        );
        assert_terminal(&shared, Error::EvidenceExpired);
    }
}

#[test]
fn opening_heartbeats_do_not_extend_the_initial_bound() {
    let shared = shared(Status::Opening, START, START + 100);
    let opening = protocol::Reply {
        state: State::Opening,
        until_ns: 0,
    };
    for now in [START + 1, START + 50, START + 99] {
        assert_eq!(shared.accept_with(opening, now, || Ok(now)), Ok(()));
        assert_eq!(shared.status_with(|| Ok(now)), Status::Opening);
    }
    assert_eq!(
        shared.accept_with(opening, START + 99, || Ok(START + 100)),
        Err(Error::OpeningExpired)
    );
    assert_terminal(&shared, Error::OpeningExpired);
}

#[test]
fn an_active_reply_cannot_overwrite_a_newer_status_clock_sample() {
    let shared = shared(Status::Active, START, START + VALID_NS);
    assert_eq!(shared.status_with(|| Ok(START + 100)), Status::Active);
    // This response was requested before that reader ran, but its later clock
    // sample regressed. Comparing only against `sent` would incorrectly admit it.
    assert_eq!(
        shared.accept_with(active(START + VALID_NS), START + 50, || Ok(START + 90)),
        Err(Error::Clock)
    );
    assert_terminal(&shared, Error::Clock);
}

#[test]
fn invalid_positive_replies_latch_the_failure_before_the_supervisor_runs() {
    for (sent, until, expected) in [
        (START, START, Error::EvidenceExpired),
        (START + 1, START + VALID_NS, Error::EvidenceExpired),
        (u64::MAX, u64::MAX, Error::Clock),
    ] {
        let shared = shared(Status::Active, START, START + VALID_NS);
        assert_eq!(
            shared.accept_with(active(until), sent, || Ok(START)),
            Err(expected)
        );
        assert_terminal(&shared, expected);
    }
}

#[test]
fn native_end_and_reopened_state_are_terminal_at_publication() {
    for state in [
        State::Opening,
        State::Locked,
        State::Inactive,
        State::Suspending,
        State::SessionUnavailable,
        State::IdentityChanged,
        State::ServiceUnavailable,
        State::UnsupportedSession,
        State::EvidenceExpired,
        State::Failed,
    ] {
        let shared = shared(Status::Active, START, START + VALID_NS);
        let error = if state == State::Opening {
            Error::Protocol
        } else {
            Error::Native(state)
        };
        assert_eq!(
            shared.accept_with(protocol::Reply { state, until_ns: 0 }, START, || Ok(START)),
            Err(error)
        );
        assert_terminal(&shared, error);
    }
}

#[test]
fn clock_failure_on_status_or_reply_fences_the_same_owner() {
    for response in [false, true] {
        let shared = shared(Status::Active, START, START + VALID_NS);
        if response {
            assert_eq!(
                shared.accept_with(active(START + VALID_NS), START, || Err(Error::Clock)),
                Err(Error::Clock)
            );
        } else {
            assert_eq!(
                shared.status_with(|| Err(Error::Clock)),
                Status::Stopped(Error::Clock)
            );
        }
        assert_terminal(&shared, Error::Clock);
    }
}

#[test]
fn poisoned_evidence_cannot_remain_active_or_be_renewed() {
    let shared = Arc::new(shared(Status::Active, START, START + VALID_NS));
    let writer = shared.clone();
    assert!(
        thread::spawn(move || {
            let _held = writer.evidence.lock().unwrap();
            panic!("injected interrupted publication");
        })
        .join()
        .is_err()
    );
    assert_terminal(&shared, Error::Protocol);
}

#[test]
fn awaiting_initial_evidence_expires_without_a_child_wakeup() {
    use asupersync::{runtime::RuntimeBuilder, types::Budget};

    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let now = now_ns().unwrap();
        // Deliberately no process thread: this represents launch blocked in a
        // foreign call. Only the host's original bounded wait may wake itself.
        let control = Control(Arc::new(shared(Status::Opening, now, now + 20_000_000)));
        let result = asupersync::time::timeout(
            cx.now(),
            Duration::from_secs(1),
            crate::host_run::session_opened(&cx, &control),
        )
        .await
        .expect("the original opening deadline must wake without child progress");
        assert_eq!(
            result,
            Err(crate::host_run::Error::SessionMonitor(
                Error::OpeningExpired
            ))
        );
        assert_eq!(control.status(), Status::Stopped(Error::OpeningExpired));
    });
}
