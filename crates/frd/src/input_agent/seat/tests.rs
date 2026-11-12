//! Original seat retirement across real registration/lookup contention. Native
//! effects are covered by the separate protocol-child and traced-sink tests.
use super::*;
use crate::input_agent::{AdmissionGate, Route};
use asupersync::{
    cx::Cx,
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{
        Capabilities, InputSession, InputSink, Operation, PlatformError, Submission,
    },
    limits::ProtocolLimits,
};
#[cfg(target_os = "linux")]
use std::sync::mpsc;
use std::{
    thread,
    time::{Duration, Instant},
};

fn input(runtime: &Runtime) -> (Cx, InputSession) {
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let now = crate::input_watchdog::host_now(&cx).unwrap();
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut authority =
        SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(credentials.lease, now).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, now)
        .unwrap();
    let session = InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        now,
    )
    .unwrap();
    (cx, session)
}

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

struct NeverStarted;
impl InputSink for NeverStarted {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        panic!("retired reservation reached native preparation");
    }
    fn submit(&mut self, _: Operation) -> Submission {
        panic!("retired reservation reached native submission");
    }
}

#[test]
fn retirement_invalidates_existing_reservations_and_survives_guard_removal() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let (cx, session) = input(&runtime);
    let seat = Seat::default();
    let reservation = seat.reserve().unwrap();
    let guard = seat.inhibit().unwrap();
    seat.retire(StopReason::AuthorityEnded);
    seat.retire(StopReason::Suspended);
    drop(guard);
    let result = reservation.start(
        cx,
        session,
        Route::new(7, ProtocolLimits::ABSOLUTE),
        || -> Result<NeverStarted, PlatformError> { panic!("old reservation launched") },
        |_| true,
        AdmissionGate::default(),
    );
    assert!(matches!(result, Err(Error::SeatInhibited)));
    assert_eq!(seat.retirement_reason(), Some(StopReason::AuthorityEnded));
    assert!(!seat.is_occupied(), "no native child was launched");
    assert!(matches!(seat.clone().reserve(), Err(Error::SeatInhibited)));
    assert!(matches!(seat.inhibit(), Err(Error::SeatInhibited)));
    assert!(
        Seat::default().reserve().is_ok(),
        "a different owner is untouched"
    );
}

#[test]
fn retirement_is_latched_while_prelaunch_install_waits_for_its_writer() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let (cx, session) = input(&runtime);
    let seat = Seat::default();
    let reservation = seat.reserve().unwrap();
    // This is the only phase in which an unfenced registration writer may
    // exist: the reservation has not started any native executor.
    let writer = seat.0.active.write().unwrap();
    let installer = thread::spawn(move || {
        reservation
            .start(
                cx,
                session,
                Route::new(7, ProtocolLimits::ABSOLUTE),
                || -> Result<NeverStarted, PlatformError> { panic!("retired install launched") },
                |_| true,
                AdmissionGate::default(),
            )
            .map(|_| ())
    });
    let installing = wait_for(|| seat.0.admission.try_lock().is_err());
    let retiring = seat.clone();
    let retired = thread::spawn(move || retiring.retire(StopReason::Suspended));
    let latched = wait_for(|| seat.retirement_reason() == Some(StopReason::Suspended));
    drop(writer);
    retired.join().unwrap();
    let result = installer.join().unwrap();
    assert!(
        installing,
        "the test reached the pre-launch writer boundary"
    );
    assert!(latched, "retirement must precede the writer wait");
    assert!(matches!(result, Err(Error::SeatInhibited)));
    assert!(!seat.is_occupied());
    assert!(matches!(seat.reserve(), Err(Error::SeatInhibited)));
}

#[test]
fn retirement_calls_the_executor_fence_outside_both_seat_locks() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let (cx, session) = input(&runtime);
    let watchdog = crate::input_watchdog::Watchdog::new(cx, session.monitor()).unwrap();
    let control = watchdog.control();
    let seat = Seat::default();
    let reservation = seat.reserve().unwrap();
    seat.install(reservation.epoch, &control).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let observed = called.clone();
    let checked = seat.clone();
    assert!(control.install_fence(Box::new(move || {
        assert!(checked.0.admission.try_lock().is_ok());
        assert!(checked.0.active.try_write().is_ok());
        observed.store(true, Ordering::Release);
    })));
    seat.retire(StopReason::Suspended);
    assert!(called.load(Ordering::Acquire));
    assert!(control.is_stopped());
    assert_eq!(control.reason(), Some(StopReason::Suspended));
    drop(reservation);
    assert!(!seat.is_occupied());
}

#[cfg(target_os = "linux")]
#[test]
fn retirement_reaches_entered_native_submit_with_admission_and_reader_held() {
    let seat = Seat::default();
    crate::input_process::tests::entered_submit_stopped_by(&seat, StopReason::Suspended, |_| {
        let admission = seat.0.lock();
        let reader = seat.0.active.read().unwrap();
        let retiring = seat.clone();
        let (done, result) = mpsc::sync_channel(1);
        let stopper = thread::spawn(move || {
            retiring.retire(StopReason::Suspended);
            done.send(()).unwrap();
        });
        let stopped = result.recv_timeout(Duration::from_secs(1));
        drop(reader);
        drop(admission);
        stopper.join().unwrap();
        assert!(
            stopped.is_ok(),
            "native fence waited for an unrelated seat lock"
        );
    });
    assert_eq!(seat.retirement_reason(), Some(StopReason::Suspended));
    assert!(matches!(seat.reserve(), Err(Error::SeatInhibited)));
}
