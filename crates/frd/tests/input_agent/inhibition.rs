//! Real authority and native executor; the sink's OS effects are explicit traces.
use super::*;

#[test]
fn consent_fences_a_blocked_preflight_before_its_first_submission() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let effects = trace();
    let native = effects.clone();
    let (entered, waiting) = mpsc::sync_channel(1);
    let (resume, blocked) = mpsc::sync_channel(1);
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut sink = Sink::new(native);
                sink.block_prepare = Some((entered, blocked));
                Ok(sink)
            },
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    let fence_called = Arc::new(AtomicBool::new(false));
    let called = fence_called.clone();
    agent.control().install_fence(Box::new(move || {
        called.store(true, Ordering::Release);
    }));
    let guard = seat.inhibit().unwrap();
    assert!(fence_called.load(Ordering::Acquire));
    assert!(agent.control().is_stopped());
    assert_eq!(agent.control().reason(), Some(StopReason::Suspended));
    assert!(
        !guard.is_ready(),
        "preflight is still owned by the old native thread"
    );
    resume.send(()).unwrap();
    assert_eq!(
        submitted(reply(&mut agent)).outcome,
        InputOutcome::CancelledBeforeSubmission
    );
    assert!(rt.block_on(driver).handoff_safe());
    assert!(guard.is_ready());
    assert_eq!(*effects.effects.lock().unwrap(), []);
    assert_eq!(effects.drops.load(Ordering::Acquire), 1);
    assert_eq!(
        agent.submit(&bytes(1, key(true)), InputDelivery::Reliable),
        Err(Error::Stopped)
    );
}

#[test]
fn consent_readiness_waits_for_native_destruction_not_just_key_release() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let effects = trace();
    let native = effects.clone();
    let (entered, waiting) = mpsc::sync_channel(1);
    let (resume, blocked) = mpsc::sync_channel(1);
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut sink = Sink::new(native);
                sink.drop_block = Some((entered, blocked));
                Ok(sink)
            },
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let first = submitted(reply(&mut agent));
    assert_eq!(first.outcome, InputOutcome::SubmittedToOs);
    let guard = seat.inhibit().unwrap();
    waiting.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(
        !effects.held.load(Ordering::Acquire),
        "release already happened"
    );
    assert!(
        !guard.is_ready(),
        "destructor still owns the native resources"
    );
    assert_eq!(effects.drops.load(Ordering::Acquire), 0);
    resume.send(()).unwrap();
    assert!(rt.block_on(driver).handoff_safe());
    assert!(guard.is_ready());
    // One press and its cleanup release, never a rollback/replay of the press.
    assert_eq!(effects.effects.lock().unwrap().len(), 2);
    assert_eq!(first.submitted_operations, 1);
}

#[test]
fn overlapping_consent_guards_block_all_clones_until_the_last_is_gone() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let other = seat.clone();
    let mut guards: Vec<_> = (0..8).map(|_| seat.inhibit().unwrap()).collect();
    assert!(guards.iter().all(frd::input_agent::Inhibition::is_ready));
    assert!(matches!(other.inhibit(), Err(Error::SeatInhibited)));
    while let Some(guard) = guards.pop() {
        assert!(matches!(
            other.start(
                cx.clone(),
                session(&cx, 3_000_000),
                route(),
                || -> Result<Sink, PlatformError> {
                    panic!("inhibited native factory must never run");
                },
                clean
            ),
            Err(Error::SeatInhibited)
        ));
        drop(guard);
    }
    let (agent, driver) = other
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            || Ok(Sink::new(trace())),
            clean,
        )
        .unwrap();
    assert!(!agent.control().is_stopped());
    agent.control().stop(StopReason::LocalRevoke);
    assert!(rt.block_on(driver).handoff_safe());
}

#[test]
fn abandoning_consent_does_not_clear_uncertain_native_cleanup() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let effects = trace();
    effects.clean.store(false, Ordering::Release);
    let native = effects.clone();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(native)),
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let _ = reply(&mut agent);
    let guard = seat.inhibit().unwrap();
    eventually(|| agent.status().cleanup.is_some());
    assert!(!guard.is_ready());
    drop(guard);
    assert!(seat.is_occupied());
    drop(driver);
    eventually(|| effects.drops.load(Ordering::Acquire) == 1);
    assert!(
        seat.is_occupied(),
        "abandonment is not proof of safe handoff"
    );
    let guard = seat.inhibit().unwrap();
    assert!(!guard.is_ready(), "another prompt must still wait/refuse");
}
