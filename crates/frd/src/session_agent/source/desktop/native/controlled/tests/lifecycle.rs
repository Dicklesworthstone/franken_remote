//! Exercise the real local-priority/drain loop with a scripted managed service.
//! Session authority and cancellation are real; media/native effects are not.
use super::*;
use crate::{media::ObservationControl, session_startup::PublisherError};
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    types::Budget,
};
use fr_core::{authority::AuthorityPolicy, ids::RemoteSessionId};
use std::{cell::Cell, pin::Pin, task::Waker};

fn fixture() -> (Runtime, Cx, ObservationControl, SessionAgent) {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let mut authority = fr_core::authority::SessionAuthority::new(
        RemoteSessionId::from_raw(7),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(crate::media::host_now(&cx).unwrap())
        .unwrap();
    let observation = ObservationControl::new(cx.clone(), authority).unwrap();
    let agent = SessionAgent::new(
        ApprovalMode::Unattended,
        PlatformKind::LinuxX11,
        3,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
    );
    (runtime, cx, observation, agent)
}

struct Drain {
    control: ObservationControl,
    original: Result<(), PublisherError>,
    terminal: Option<Result<(), PublisherError>>,
    input: Option<crate::input_agent::Shutdown>,
    ready: Arc<AtomicBool>,
    must_be_revoked: bool,
    shutdown_started: bool,
    defer_session_result: bool,
}
impl Future for Drain {
    type Output = ManagedControlReport;
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if !this.shutdown_started {
            if this.must_be_revoked {
                assert!(this.control.context().is_cancel_requested());
            }
            this.control.revoke();
            this.shutdown_started = true;
            if !this.defer_session_result {
                this.terminal = Some(this.original);
            }
            return Poll::Pending;
        }
        if this.terminal.is_none() {
            this.terminal = Some(this.original);
            return Poll::Pending;
        }
        if !this.ready.load(Ordering::Acquire) {
            return Poll::Pending;
        }
        Poll::Ready(ManagedControlReport {
            session: this.original,
            input: this.input,
            revocation: None,
        })
    }
}
impl ManagedControlService for Drain {
    fn shutdown_started(&self) -> bool {
        self.shutdown_started
    }
    fn terminal_session(&self) -> Option<Result<(), PublisherError>> {
        self.terminal
    }
}
fn service(
    observation: &ObservationControl,
    session: Result<(), PublisherError>,
    must_be_revoked: bool,
) -> (Drain, Arc<AtomicBool>) {
    let ready = Arc::new(AtomicBool::new(false));
    (
        Drain {
            control: observation.clone(),
            original: session,
            terminal: None,
            input: None,
            ready: ready.clone(),
            must_be_revoked,
            shutdown_started: false,
            defer_session_result: false,
        },
        ready,
    )
}
fn session_failure() -> PublisherError {
    PublisherError::Transport(fr_transport::quic::Error::Stream(
        fr_wire::stream::StreamError::Expired,
    ))
}

#[test]
fn teardown_cancellation_cannot_overwrite_an_already_terminal_session() {
    for session in [Ok(()), Err(session_failure())] {
        let (_runtime, cx, observation, mut agent) = fixture();
        let (service, ready) = service(&observation, session, false);
        let calls = Cell::new(0);
        let checked = cx.clone();
        let mut drive = pin!(drive_control(
            &mut agent,
            &cx,
            |_, _| {
                calls.set(calls.get() + 1);
                checked
                    .checkpoint()
                    .map_err(|_| LocalFailure::SourceLifetimeCancelled)?;
                Ok(LocalAction::Continue)
            },
            Arc::new(AtomicBool::new(true)),
            observation,
            None,
            service,
        ));
        let mut task = Context::from_waker(Waker::noop());
        assert!(drive.as_mut().poll(&mut task).is_pending());
        assert!(cx.is_cancel_requested(), "session ended before its drain");
        assert!(drive.as_mut().poll(&mut task).is_pending());
        assert_eq!(calls.get(), 1, "do not recheck a fenced session as live");
        ready.store(true, Ordering::Release);
        assert_eq!(
            drive.as_mut().poll(&mut task),
            Poll::Ready(session.map(|()| served(false)).map_err(Error::Publication))
        );
    }
}

#[test]
fn native_fence_with_a_pending_session_result_does_not_reenter_local_checks() {
    let (_runtime, cx, observation, mut agent) = fixture();
    let (mut service, ready) = service(&observation, Err(session_failure()), false);
    service.defer_session_result = true;
    let calls = Cell::new(0);
    let checked = cx.clone();
    let mut drive = pin!(drive_control(
        &mut agent,
        &cx,
        |_, _| {
            calls.set(calls.get() + 1);
            checked
                .checkpoint()
                .map_err(|_| LocalFailure::SourceLifetimeCancelled)?;
            Ok(LocalAction::Continue)
        },
        Arc::new(AtomicBool::new(true)),
        observation,
        None,
        service,
    ));
    let mut task = Context::from_waker(Waker::noop());
    assert!(drive.as_mut().poll(&mut task).is_pending());
    assert!(
        cx.is_cancel_requested(),
        "native stop fenced before I/O returned"
    );
    assert!(drive.as_mut().poll(&mut task).is_pending());
    assert_eq!(calls.get(), 1, "the result can lag behind the fence");
    ready.store(true, Ordering::Release);
    assert_eq!(
        drive.as_mut().poll(&mut task),
        Poll::Ready(Err(Error::Publication(session_failure())))
    );
}

#[test]
fn independent_local_failure_fences_first_and_retains_both_observed_causes() {
    for cause in [
        LocalFailure::SourceLifetimeCancelled,
        LocalFailure::DispatcherClosed,
        LocalFailure::DispatcherPoisoned,
        LocalFailure::CallbackFailed,
    ] {
        let (_runtime, cx, observation, mut agent) = fixture();
        let (service, ready) = service(&observation, Err(session_failure()), true);
        let mut drive = pin!(drive_control(
            &mut agent,
            &cx,
            |_, _| Err(cause),
            Arc::new(AtomicBool::new(true)),
            observation,
            None,
            service,
        ));
        let mut task = Context::from_waker(Waker::noop());
        assert!(drive.as_mut().poll(&mut task).is_pending());
        assert!(cx.is_cancel_requested());
        ready.store(true, Ordering::Release);
        assert_eq!(
            drive.as_mut().poll(&mut task),
            Poll::Ready(Err(Error::LocalFailure {
                cause,
                session: Some(session_failure()),
            }))
        );
    }
}

#[test]
fn requested_local_stop_keeps_its_reason_and_finishes_the_original_drain() {
    let (_runtime, cx, observation, mut agent) = fixture();
    let retained = observation.clone();
    agent.note_local_stop(StopReason::Suspended);
    let (service, ready) = service(&observation, Err(session_failure()), true);
    let mut drive = pin!(drive_control(
        &mut agent,
        &cx,
        |_, _| Ok(LocalAction::Stop),
        Arc::new(AtomicBool::new(true)),
        observation,
        None,
        service,
    ));
    let mut task = Context::from_waker(Waker::noop());
    assert!(drive.as_mut().poll(&mut task).is_pending());
    assert_eq!(retained.end_reason(), Some(StopReason::Suspended));
    ready.store(true, Ordering::Release);
    assert_eq!(
        drive.as_mut().poll(&mut task),
        Poll::Ready(Ok(served(false)))
    );
}

#[test]
fn uncertain_input_cleanup_outranks_both_local_and_session_errors() {
    for local_failure in [false, true] {
        let (_runtime, cx, observation, mut agent) = fixture();
        let (mut service, ready) = service(&observation, Err(session_failure()), local_failure);
        service.input = Some(crate::input_agent::Shutdown {
            reason: StopReason::Cancelled,
            exit: None,
            last_cleanup: None,
        });
        let mut drive = pin!(drive_control(
            &mut agent,
            &cx,
            |_, _| {
                if local_failure {
                    Err(LocalFailure::CallbackFailed)
                } else {
                    Ok(LocalAction::Continue)
                }
            },
            Arc::new(AtomicBool::new(true)),
            observation,
            None,
            service,
        ));
        let mut task = Context::from_waker(Waker::noop());
        assert!(drive.as_mut().poll(&mut task).is_pending());
        ready.store(true, Ordering::Release);
        assert_eq!(
            drive.as_mut().poll(&mut task),
            Poll::Ready(Err(Error::InputCleanup))
        );
    }
}
