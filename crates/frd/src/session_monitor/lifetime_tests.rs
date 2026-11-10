//! Original monitor evidence under injected clock values. These exercise the
//! production publication/expiry logic, not native desktop lifecycle events.
use super::*;
use std::time::Duration;

const START: u64 = 1_000_000_000;

fn shared(status: Status, last_ns: u64, until_ns: u64) -> Shared {
    Shared {
        evidence: Mutex::new(Evidence {
            status,
            until_ns,
            last_ns,
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
fn pending_setup_is_woken_and_fenced_without_repolling_its_original_work() {
    use std::{cell::Cell, future::poll_fn, pin::pin, sync::atomic::AtomicUsize, task::Wake};

    struct Wakes(AtomicUsize);
    impl Wake for Wakes {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let now = now_ns().unwrap();
    let control = Control(Arc::new(shared(Status::Active, now, now + OPEN_NS)));
    let wakes = Arc::new(Wakes(AtomicUsize::new(0)));
    let waker = Waker::from(wakes.clone());
    let mut task = Context::from_waker(&waker);
    let polls = Cell::new(0);
    let mut work = pin!(poll_fn(|_| {
        polls.set(polls.get() + 1);
        Poll::<()>::Pending
    }));
    assert_eq!(control.poll_setup(&mut task, work.as_mut()), Poll::Pending);
    assert_eq!(polls.get(), 1);
    control.0.stop(Error::Native(State::Inactive));
    assert_eq!(wakes.0.load(Ordering::Relaxed), 1);
    assert_eq!(
        control.poll_setup(&mut task, work.as_mut()),
        Poll::Ready(Err(Error::Native(State::Inactive)))
    );
    assert_eq!(polls.get(), 1, "ended setup must never poll work again");
}

#[test]
fn evidence_expiry_ends_pending_setup_without_a_native_notification() {
    use std::{cell::Cell, future::poll_fn, pin::pin};

    let now = now_ns().unwrap();
    let control = Control(Arc::new(shared(Status::Active, now, now + OPEN_NS)));
    let mut task = Context::from_waker(Waker::noop());
    let polls = Cell::new(0);
    let mut work = pin!(poll_fn(|_| {
        polls.set(polls.get() + 1);
        Poll::<()>::Pending
    }));
    assert_eq!(control.poll_setup(&mut task, work.as_mut()), Poll::Pending);
    // The host's independent timer supplies this next poll. No native end or
    // wake has been published, but the original evidence is now expired.
    control.0.evidence().until_ns = now_ns().unwrap();
    assert_eq!(
        control.poll_setup(&mut task, work.as_mut()),
        Poll::Ready(Err(Error::EvidenceExpired))
    );
    assert_eq!(polls.get(), 1);
    assert_terminal(&control.0, Error::EvidenceExpired);
}

#[test]
fn terminal_evidence_beats_setup_that_is_ready_in_the_same_turn() {
    use std::{cell::Cell, pin::pin};

    struct ReadyOwner<'a>(&'a Cell<usize>);
    impl Drop for ReadyOwner<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    for already_ended in [true, false] {
        let now = now_ns().unwrap();
        let control = Control(Arc::new(shared(Status::Active, now, now + OPEN_NS)));
        let mut task = Context::from_waker(Waker::noop());
        let polled = Cell::new(false);
        let dropped = Cell::new(0);
        if already_ended {
            control.0.stop(Error::Native(State::Suspending));
        }
        let mut work = pin!(async {
            polled.set(true);
            control.0.stop(Error::Native(State::Suspending));
            ReadyOwner(&dropped)
        });
        assert!(matches!(
            control.poll_setup(&mut task, work.as_mut()),
            Poll::Ready(Err(Error::Native(State::Suspending)))
        ));
        assert_eq!(polled.get(), !already_ended);
        assert_eq!(dropped.get(), usize::from(!already_ended));
    }
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

#[test]
fn a_native_stop_during_startup_beats_ready_certificate_or_policy_work() {
    use asupersync::runtime::RuntimeBuilder;
    use std::cell::Cell;

    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    for state in [
        Status::Stopped(Error::Native(State::Locked)),
        Status::Active,
    ] {
        // The Active case has already expired on the shared kernel clock.
        let control = Control(Arc::new(shared(state, 0, 1)));
        let stop = crate::host_run::StopHandle::default();
        let mut handled = (None, None);
        let mut signals = crate::signals::Watch::new(&mut handled);
        let polled = Cell::new(false);
        let result = runtime.block_on(crate::host_run::unless_stopped(
            async {
                polled.set(true);
                7
            },
            &stop,
            &mut signals,
            Some(&control),
        ));
        assert_eq!(result, None);
        assert!(
            !polled.get(),
            "stale native evidence must win before network work"
        );
        assert!(stop.is_requested());
    }
}

#[test]
fn pending_host_setup_expires_without_a_child_wakeup() {
    use asupersync::{runtime::RuntimeBuilder, types::Budget};

    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let now = now_ns().unwrap();
        let control = Control(Arc::new(shared(Status::Active, now, now + 20_000_000)));
        let result = asupersync::time::timeout(
            cx.now(),
            Duration::from_secs(1),
            crate::host_run::session_setup(&cx, Some(&control), std::future::pending::<()>()),
        )
        .await
        .expect("the original setup wait must expire without child progress");
        assert_eq!(
            result,
            Err(crate::host_run::Error::SessionEnded(Error::EvidenceExpired))
        );
        assert_eq!(control.status(), Status::Stopped(Error::EvidenceExpired));
    });
}
