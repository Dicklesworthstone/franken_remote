//! Original monitor registration and native-input fencing, without polling a
//! host service future. Native notifications are explicit injected evidence.
use super::*;
use std::sync::mpsc;
#[cfg(target_os = "linux")]
use std::time::Instant;

fn current() -> (u64, Control) {
    let now = now_ns().unwrap();
    (
        now,
        Control(Arc::new(shared(Status::Active, now, now + VALID_NS))),
    )
}

#[cfg(target_os = "linux")]
fn wait_for(mut ready: impl FnMut() -> bool) -> bool {
    let until = Instant::now() + Duration::from_secs(2);
    while !ready() {
        if Instant::now() >= until {
            return false;
        }
        thread::yield_now();
    }
    true
}

#[test]
fn terminal_registration_retires_the_exact_seat_and_cannot_replace_it() {
    let (_, monitor) = current();
    let original = Seat::default();
    monitor.bind_input(&original).unwrap();
    monitor.bind_input(&original.clone()).unwrap();
    let other = Seat::default();
    assert_eq!(monitor.bind_input(&other), Err(Error::Selection));
    assert_eq!(other.retirement_reason(), Some(StopReason::AuthorityEnded));
    assert_eq!(original.retirement_reason(), None);
    monitor.0.stop(Error::Native(State::Inactive));
    assert_eq!(
        original.retirement_reason(),
        Some(StopReason::AuthorityEnded)
    );
    assert_eq!(
        monitor.bind_input(&original),
        Err(Error::Native(State::Inactive))
    );
    let (_, new_monitor) = current();
    assert_eq!(new_monitor.bind_input(&original), Err(Error::Stopped));
    assert!(matches!(
        original.reserve(),
        Err(crate::input_agent::Error::SeatInhibited)
    ));
}

#[test]
fn native_stop_before_registration_never_becomes_a_fresh_grant() {
    let (_, monitor) = current();
    monitor.0.stop(Error::Native(State::Suspending));
    let seat = Seat::default();
    assert_eq!(
        monitor.bind_input(&seat),
        Err(Error::Native(State::Suspending))
    );
    assert_eq!(seat.retirement_reason(), Some(StopReason::Suspended));
    assert!(matches!(
        seat.reserve(),
        Err(crate::input_agent::Error::SeatInhibited)
    ));
    assert_terminal(&monitor.0, Error::Native(State::Suspending));
}

#[test]
fn stop_does_not_wait_for_an_unfinished_registration_and_the_registrar_rechecks() {
    let (_, monitor) = current();
    let seat = Seat::default();
    let registering = monitor.clone();
    let registered_seat = seat.clone();
    let (entered, waiting) = mpsc::sync_channel(1);
    let (resume, held) = mpsc::sync_channel(1);
    let registrar = thread::spawn(move || {
        // Hold the actual one-use cell in its initialization phase. A terminal
        // producer must not wait here; the registering API rechecks after set.
        registering.0.input.get_or_init(|| {
            entered.send(()).unwrap();
            held.recv_timeout(Duration::from_secs(3)).unwrap();
            registered_seat.clone()
        });
        registering.bind_input(&registered_seat)
    });
    waiting.recv_timeout(Duration::from_secs(1)).unwrap();
    let stopping = monitor.clone();
    let (done, stopped) = mpsc::sync_channel(1);
    let stopper = thread::spawn(move || {
        done.send(stopping.0.stop(Error::Native(State::SessionUnavailable)))
            .unwrap();
    });
    let result = stopped.recv_timeout(Duration::from_secs(1));
    resume.send(()).unwrap();
    stopper.join().unwrap();
    let bound = registrar.join().unwrap();
    assert_eq!(result.unwrap(), Error::Native(State::SessionUnavailable));
    assert_eq!(bound, Err(Error::Native(State::SessionUnavailable)));
    assert_eq!(seat.retirement_reason(), Some(StopReason::AuthorityEnded));
    assert!(matches!(
        seat.reserve(),
        Err(crate::input_agent::Error::SeatInhibited)
    ));
}

#[test]
fn first_native_cause_survives_a_poisoned_reader_and_later_clock_failures() {
    let (_, monitor) = current();
    let seat = Seat::default();
    monitor.bind_input(&seat).unwrap();
    let reader = monitor.clone();
    let (entered, waiting) = mpsc::sync_channel(1);
    let (resume, held) = mpsc::sync_channel(1);
    let poison = thread::spawn(move || {
        let _guard = reader.0.evidence.lock().unwrap();
        entered.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(3)).unwrap();
        panic!("injected interrupted evidence reader");
    });
    waiting.recv_timeout(Duration::from_secs(1)).unwrap();
    let stopping = monitor.clone();
    let (done, stopped) = mpsc::sync_channel(1);
    let stopper = thread::spawn(move || {
        done.send(stopping.0.stop(Error::Native(State::Inactive)))
            .unwrap();
    });
    let result = stopped.recv_timeout(Duration::from_secs(1));
    resume.send(()).unwrap();
    assert!(poison.join().is_err());
    stopper.join().unwrap();
    assert_eq!(result.unwrap(), Error::Native(State::Inactive));
    assert_eq!(seat.retirement_reason(), Some(StopReason::AuthorityEnded));
    assert_eq!(
        monitor.0.status_with(|| Err(Error::Clock)),
        Status::Stopped(Error::Native(State::Inactive))
    );
    assert_eq!(monitor.0.deadline(), Err(Error::Native(State::Inactive)));
    assert_terminal(&monitor.0, Error::Native(State::Inactive));
}

#[test]
fn expired_or_failed_evidence_fences_before_late_positive_and_native_replies() {
    for error in [Error::EvidenceExpired, Error::Clock, Error::Protocol] {
        let (now, monitor) = current();
        let seat = Seat::default();
        monitor.bind_input(&seat).unwrap();
        let status = if error == Error::EvidenceExpired {
            monitor.0.status_with(|| Ok(now + VALID_NS))
        } else {
            monitor.0.status_with(|| Err(error))
        };
        assert_eq!(status, Status::Stopped(error));
        assert_eq!(seat.retirement_reason(), Some(StopReason::AuthorityEnded));
        assert_eq!(
            monitor.0.accept_with(
                protocol::Reply {
                    state: State::Suspending,
                    until_ns: 0
                },
                now,
                || panic!("a terminal monitor sampled a later clock"),
            ),
            Err(error)
        );
        assert_terminal(&monitor.0, error);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn native_stop_fences_entered_child_submit_before_busy_evidence_and_wake_locks() {
    for state in [
        State::Suspending,
        State::Inactive,
        State::SessionUnavailable,
    ] {
        let (now, monitor) = current();
        let seat = Seat::default();
        monitor.bind_input(&seat).unwrap();
        let error = Error::Native(state);
        crate::input_process::tests::entered_submit_stopped_by(
            &seat,
            error.input_stop_reason(),
            |log| {
                let evidence = monitor.0.evidence.lock().unwrap();
                let wake = monitor.0.waker.lock().unwrap();
                let stopping = monitor.clone();
                let stopper = thread::spawn(move || {
                    stopping
                        .0
                        .accept_with(protocol::Reply { state, until_ns: 0 }, now, || {
                            panic!("negative evidence waited for a clock")
                        })
                });
                // This is actual child IPC evidence, not merely a parent atomic
                // flag: the entered submit has observed its original fence.
                let fenced = wait_for(|| {
                    let lines = crate::input_process::tests::transcript(log);
                    lines.iter().any(|line| line == "FENCE")
                        && lines.iter().any(|line| line.starts_with("SUBMIT-FENCED"))
                });
                drop(wake);
                drop(evidence);
                assert_eq!(stopper.join().unwrap(), Err(error));
                assert!(fenced, "native fence waited behind a monitor mutex");
            },
        );
        assert_terminal(&monitor.0, error);
    }
}
