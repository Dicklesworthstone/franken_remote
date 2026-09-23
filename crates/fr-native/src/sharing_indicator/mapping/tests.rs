//! Deterministic lifecycle fixtures. Real mapping is covered in the Xvfb suite.
use super::*;
use crate::sharing_indicator::{IndicatorControl, Shared};
use asupersync::{
    runtime::{Runtime, RuntimeBuilder},
    time::{TimerDriverHandle, VirtualClock},
    types::{Budget, CancelKind},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::RemoteSessionId,
    time::HostInstant,
};
use frd::media::ObservationControl;
use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicU32},
    },
    task::{Context, Poll, Waker},
};
fn fixture(
    status: Status,
) -> (
    Runtime,
    Arc<VirtualClock>,
    Cx,
    SharingIndicator,
    ObservationControl,
) {
    let clock = Arc::new(VirtualClock::new());
    let rt = RuntimeBuilder::current_thread()
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(clock.clone()))
        .build()
        .unwrap();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let source = rt.request_cx_with_budget(Budget::INFINITE);
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(HostInstant::from_micros(0))
        .unwrap();
    let observation = ObservationControl::new(source, authority).unwrap();
    let indicator = SharingIndicator {
        control: IndicatorControl(Arc::new(Shared {
            state: AtomicU8::new(match status {
                Status::Opening => 0,
                Status::Mapped => 1,
                Status::Stopped(reason) => reason as u8,
            }),
            observation: observation.clone(),
            window: AtomicU32::new(0),
        })),
        task: None,
    };
    (rt, clock, cx, indicator, observation)
}
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}
#[test]
fn mapped_wait_preserves_source_and_indicator_ownership() {
    let (_rt, _, cx, mut indicator, observation) = fixture(Status::Mapped);
    let mut waiting = Box::pin(indicator.wait_until_mapped(&cx));
    assert_eq!(poll(waiting.as_mut()), Poll::Ready(Ok(())));
    drop(waiting);
    assert!(observation.check().is_ok());
    assert_eq!(indicator.control.status(), Status::Mapped);
}
#[test]
fn unpolled_mapping_wait_abandonment_revokes_only_original_source() {
    let (_rt, _, cx, mut indicator, observation) = fixture(Status::Opening);
    drop(indicator.wait_until_mapped(&cx));
    assert!(observation.check().is_err());
    assert!(cx.checkpoint().is_ok());
    assert_eq!(
        indicator.control.status(),
        Status::Stopped(StopReason::OwnerDropped)
    );
}
#[test]
fn call_time_deadline_refuses_even_a_late_ready_mapping() {
    let (_rt, clock, cx, mut indicator, observation) = fixture(Status::Mapped);
    let mut waiting = Box::pin(indicator.wait_until_mapped(&cx));
    clock.advance(2_000_000_000);
    assert_eq!(
        poll(waiting.as_mut()),
        Poll::Ready(Err(Error::Stopped(StopReason::MappingExpired)))
    );
    assert!(observation.check().is_err());
    assert!(cx.checkpoint().is_ok());
}
#[test]
fn opening_waits_but_cancellation_overrides_ready_mapping() {
    for status in [Status::Opening, Status::Mapped] {
        let (_rt, _, cx, mut indicator, observation) = fixture(status);
        let mut waiting = Box::pin(indicator.wait_until_mapped(&cx));
        if status == Status::Opening {
            assert!(poll(waiting.as_mut()).is_pending());
        }
        cx.cancel_fast(CancelKind::User);
        assert_eq!(
            poll(waiting.as_mut()),
            Poll::Ready(Err(Error::Stopped(StopReason::User)))
        );
        assert!(observation.check().is_err());
    }
}
#[test]
fn original_native_stop_cause_survives_the_wait_cleanup() {
    let (_rt, _, cx, mut indicator, observation) = fixture(Status::Opening);
    indicator.control.0.stop(StopReason::User);
    let mut waiting = Box::pin(indicator.wait_until_mapped(&cx));
    assert_eq!(
        poll(waiting.as_mut()),
        Poll::Ready(Err(Error::Stopped(StopReason::User)))
    );
    drop(waiting);
    assert!(observation.check().is_err());
    assert!(cx.checkpoint().is_ok());
    assert_eq!(
        indicator.control.status(),
        Status::Stopped(StopReason::User)
    );
}
#[test]
fn stopped_source_cannot_become_ready_through_a_mapped_window() {
    let (_rt, _, cx, mut indicator, observation) = fixture(Status::Mapped);
    observation.revoke();
    let mut waiting = Box::pin(indicator.wait_until_mapped(&cx));
    assert_eq!(
        poll(waiting.as_mut()),
        Poll::Ready(Err(Error::Stopped(StopReason::AuthorityEnded)))
    );
    assert!(cx.checkpoint().is_ok());
}
