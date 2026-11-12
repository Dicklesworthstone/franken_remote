use asupersync::{
    runtime::RuntimeBuilder,
    time::{TimerDriverHandle, VirtualClock},
    types::{Budget, Time},
};
use fr_core::{ids::InputTicketId, input::InputEvent, input_sequence::InputOutcome};
use fr_wire::input::{InputDelivery, MAX_INPUT_RECORD_BYTES};
#[cfg(target_os = "linux")]
use frd::input_watchdog::host_now;
use frd::{
    input_agent::{AuthorityCommand, Error, Reply, Seat},
    input_watchdog::StopReason,
};
use std::{
    sync::{Arc, atomic::Ordering, mpsc},
    time::Duration,
};

#[path = "support/input_agent.rs"]
#[allow(dead_code)] // Each suite consumes a different subset of the shared fixture.
mod support;
use support::*;

#[test]
#[cfg(target_os = "linux")]
fn microphone_handoff_tracks_the_moved_input_owner_and_local_stop() {
    let runtime = runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let original = session(&cx, 3_000_000);
    let trace_state = trace();
    let seat = Seat::default();
    let (agent, driver) = seat
        .start(
            cx.clone(),
            original,
            route(),
            move || Ok(Sink::new(trace_state)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(runtime, driver);
    let controller = agent.microphone_controller().unwrap();
    assert_eq!(controller.scope().session, credentials().session);
    assert_eq!(controller.scope().lease, credentials().lease);
    assert!(controller.deadline(host_now(&cx).unwrap()).is_ok());
    agent.control().stop(StopReason::LocalRevoke);
    assert!(agent.microphone_controller().is_none());
    assert!(controller.deadline(host_now(&cx).unwrap()).is_err());
    assert!(runner.finish().handoff_safe());
    assert!(!seat.is_occupied());
}

#[test]
fn idle_expiry_releases_without_another_record_then_allows_new_controller() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 200_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(rt, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(
        submitted(reply(&mut a)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert!(trace_state.held.load(Ordering::Acquire));
    let s = runner.finish();
    assert!(s.handoff_safe());
    assert_eq!(s.reason, StopReason::AuthorityEnded);
    assert!(!trace_state.held.load(Ordering::Acquire));
    assert_eq!(trace_state.drops.load(Ordering::Acquire), 1);
    assert!(!seat.is_occupied());
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let other = trace();
    let (b, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(other)),
            clean,
        )
        .unwrap();
    a.control().stop(StopReason::LocalRevoke);
    assert!(!b.control().is_stopped());
    b.control().stop(StopReason::LocalRevoke);
    assert!(Runner::start(rt, d).finish().handoff_safe());
}
#[test]
fn one_outstanding_includes_an_uncollected_reply_and_old_actions_never_retry() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| trace_state.held.load(Ordering::Acquire));
    assert_eq!(
        a.submit(&bytes(1, key(false)), InputDelivery::Reliable),
        Err(Error::Backpressure)
    );
    let first = submitted(reply(&mut a));
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(submitted(reply(&mut a)), first);
    assert_eq!(trace_state.effects.lock().unwrap().len(), 1);
    a.control().stop(StopReason::ClientDisconnected);
    assert!(runner.finish().handoff_safe());
}
#[test]
fn blocked_native_preparation_does_not_retain_authority_or_allow_handoff() {
    let clock = Arc::new(VirtualClock::new());
    let timer = TimerDriverHandle::with_virtual_clock(clock.clone());
    let r = RuntimeBuilder::new()
        .worker_threads(1)
        .with_timer_driver(timer.clone())
        .build()
        .unwrap();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (tx, rx) = mpsc::sync_channel(1);
    let (resume, wait) = mpsc::sync_channel(1);
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 100_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.block_prepare = Some((tx, wait));
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    // Native preparation must enter while its original 50 ms ticket is live.
    // Expire the 100 ms authority only after that synchronization point, so
    // runner scheduling cannot turn this into a pre-admission refusal test.
    assert!(!a.control().is_stopped());
    clock.advance_to(Time::from_millis(100));
    let _ = timer.process_timers();
    eventually(|| a.control().reason().is_some());
    assert_eq!(a.control().reason(), Some(StopReason::AuthorityEnded));
    assert!(a.control().is_stopped());
    assert!(seat.is_occupied());
    let other = runtime();
    let other_cx = other.request_cx_with_budget(Budget::INFINITE);
    assert!(matches!(
        seat.start(
            other_cx.clone(),
            session(&other_cx, 3_000_000),
            route(),
            move || Ok(Sink::new(trace())),
            clean
        ),
        Err(Error::SeatBusy)
    ));
    resume.send(()).unwrap();
    assert_eq!(
        submitted(reply(&mut a)).outcome,
        InputOutcome::CancelledBeforeSubmission
    );
    assert!(runner.finish().handoff_safe());
    assert!(trace_state.effects.lock().unwrap().is_empty());
}
#[test]
fn blocked_entered_native_call_times_out_honestly_and_later_retains_its_receipt() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (tx, rx) = mpsc::sync_channel(1);
    let (resume, wait) = mpsc::sync_channel(1);
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.block_submit = Some((tx, wait));
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    a.control().stop(StopReason::LocalRevoke);
    let s = runner.finish();
    assert!(!s.handoff_safe());
    assert!(s.exit.is_none());
    assert!(seat.is_occupied());
    resume.send(()).unwrap();
    let receipt = submitted(reply(&mut a));
    assert_eq!(receipt.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(receipt.submitted_operations, 1);
    eventually(|| !seat.is_occupied());
    assert!(!trace_state.held.load(Ordering::Acquire));
    assert_eq!(trace_state.effects.lock().unwrap().len(), 2);
}
#[test]
fn native_cleanup_failure_quarantines_until_explicit_local_retry() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    trace_state.clean.store(false, Ordering::Release);
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let _ = reply(&mut a);
    a.control().stop(StopReason::ViewInvalidated);
    eventually(|| a.status().cleanup.is_some());
    assert_eq!(a.status().cleanup.unwrap().remaining, 0);
    assert!(!a.status().native_clean);
    assert!(seat.is_occupied());
    let s = runner.finish();
    assert!(!s.handoff_safe());
    assert!(s.exit.is_none());
    trace_state.clean.store(true, Ordering::Release);
    a.retry_cleanup().unwrap();
    eventually(|| !seat.is_occupied());
    assert!(a.status().exit.unwrap().handoff_safe());
}
#[test]
fn controller_reservation_survives_blocking_native_destructor() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (tx, rx) = mpsc::sync_channel(1);
    let (resume, wait) = mpsc::sync_channel(1);
    let (ready, constructed) = mpsc::sync_channel(1);
    let (a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.drop_block = Some((tx, wait));
                ready.send(()).unwrap();
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    // This case owns a constructed sink with an entered destructor later.
    // Cancellation before factory entry correctly creates no sink to destroy.
    constructed.recv_timeout(Duration::from_secs(2)).unwrap();
    a.control().stop(StopReason::Suspended);
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(seat.is_occupied());
    assert!(a.status().exit.is_none());
    resume.send(()).unwrap();
    assert!(runner.finish().handoff_safe());
    assert_eq!(trace_state.drops.load(Ordering::Acquire), 1);
}
#[test]
fn dropping_an_unpolled_response_fences_but_does_not_erase_a_completed_receipt() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(factory)),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    eventually(|| trace_state.held.load(Ordering::Acquire));
    drop(a.response());
    assert!(a.control().is_stopped());
    assert_eq!(
        submitted(reply(&mut a)).outcome,
        InputOutcome::SubmittedToOs
    );
    assert!(runner.finish().handoff_safe());
}
#[test]
fn native_panic_preserves_a_text_prefix_instead_of_fabricating_rollback() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let trace_state = trace();
    let factory = trace_state.clone();
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let mut s = Sink::new(factory);
                s.panic_on_second = true;
                Ok(s)
            },
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.submit(&bytes(0, InputEvent::Text("ab")), InputDelivery::Reliable)
        .unwrap();
    let Reply::NativePanic { receipt: Some(r) } = reply(&mut a) else {
        panic!("retained panic receipt required")
    };
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(
        trace_state
            .effects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        1
    );
    assert!(a.control().is_stopped());
    assert!(runner.finish().handoff_safe());
}
#[test]
fn malformed_oversized_and_wrong_channel_records_do_not_consume_the_slot() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(trace())),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    assert!(matches!(
        a.submit(&[0; 16], InputDelivery::Reliable),
        Err(Error::Wire(_))
    ));
    assert_eq!(
        a.submit(
            &vec![0; MAX_INPUT_RECORD_BYTES + 1],
            InputDelivery::Reliable
        ),
        Err(Error::RecordTooLarge)
    );
    assert!(matches!(
        a.submit(&bytes(0, key(true)), InputDelivery::Datagram),
        Err(Error::Wire(_))
    ));
    assert!(!a.status().outstanding);
    a.submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let _ = reply(&mut a);
    a.control().stop(StopReason::LocalRevoke);
    assert!(runner.finish().handoff_safe());
}
#[test]
fn local_authority_commands_use_the_session_clock_without_implicit_traffic_renewal() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let (mut a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Sink::new(trace())),
            clean,
        )
        .unwrap();
    let runner = Runner::start(r, d);
    a.authority(AuthorityCommand::ControlChallenge(17)).unwrap();
    let Reply::Authority(Ok(until)) = reply(&mut a) else {
        panic!("expected challenge")
    };
    a.authority(AuthorityCommand::ControlResponse(17)).unwrap();
    assert_eq!(reply(&mut a), Reply::Authority(Ok(until)));
    a.control().stop(StopReason::LocalRevoke);
    assert_eq!(
        a.authority(AuthorityCommand::Ticket(InputTicketId::from_raw(4))),
        Err(Error::Stopped)
    );
    assert!(runner.finish().handoff_safe());
}
#[test]
fn driver_drop_before_first_poll_stops_the_native_owner_and_cleans_up() {
    let r = runtime();
    let cx = r.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let trace_state = trace();
    let factory = trace_state.clone();
    let (ready, constructed) = mpsc::sync_channel(1);
    let (a, d) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || {
                let sink = Sink::new(factory);
                ready.send(()).unwrap();
                Ok(sink)
            },
            clean,
        )
        .unwrap();
    // Native initialization is independent of Driver polling. Establish that
    // this case has a real sink before abandoning the still-unpolled Driver.
    constructed.recv_timeout(Duration::from_secs(2)).unwrap();
    drop(d);
    assert!(a.control().is_stopped());
    assert_eq!(a.control().reason(), Some(StopReason::ClientDisconnected));
    eventually(|| !seat.is_occupied());
    assert_eq!(trace_state.drops.load(Ordering::Acquire), 1);
}

#[test]
fn await_response_and_watchdog_together_on_the_real_runtime() {
    use std::{
        future::{Future, poll_fn},
        pin::Pin,
        task::Poll,
    };
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let seat = Seat::default();
    let (mut agent, mut driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            || Ok(Sink::new(trace())),
            clean,
        )
        .unwrap();
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    let mut response = agent.response();
    let result = rt
        .block_on(poll_fn(|task| {
            assert!(Pin::new(&mut driver).poll(task).is_pending());
            Pin::new(&mut response).poll(task)
        }))
        .unwrap();
    drop(response);
    assert_eq!(submitted(result).outcome, InputOutcome::SubmittedToOs);
    agent.control().stop(StopReason::LocalRevoke);
    let shutdown = rt.block_on(poll_fn(|task| match Pin::new(&mut driver).poll(task) {
        Poll::Ready(report) => Poll::Ready(report),
        Poll::Pending => Poll::Pending,
    }));
    assert!(shutdown.handoff_safe());
}

#[path = "input_agent/inhibition.rs"]
mod inhibition;

#[path = "input_agent/geometry.rs"]
mod geometry;
