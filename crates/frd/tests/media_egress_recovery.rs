#![cfg(target_os = "linux")]
//! Egress admission and authority tests, not codec or live-network qualification.
use asupersync::{cx::Cx, runtime::RuntimeBuilder, time::sleep, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::{DeliveryError, MediaBindings, MediaEpoch, SendError, SendPolicy},
};
use fr_wire::MediaLimits;
use frd::{
    media::{Error, ObservationControl, Subscription, host_now},
    media_egress::{Admission, Egress, EgressError, Lane, Progress},
};
use std::time::Duration;

const RECOVERY_US: u64 = 100_000;
fn epoch() -> MediaEpoch {
    MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn setup(cx: &Cx) -> (ObservationControl, Egress) {
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(9),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(2_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    let now = host_now(cx).unwrap();
    authority.authorize_observation(now).unwrap();
    // Synthetic readiness establishes the before/after authority assertion;
    // it is deliberately not evidence that an OS decoder presented anything.
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(InputLeaseId::from_raw(1), now).unwrap();
    authority.mark_view_ready(now).unwrap();
    let control = ObservationControl::new(cx.clone(), authority).unwrap();
    let subscription = Subscription::new(
        control.clone(),
        MediaLimits::new(ProtocolLimits::ABSOLUTE, 1024, 16_384, 64).unwrap(),
        MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch(),
        SendPolicy {
            recovery_horizon_micros: RECOVERY_US,
            ..SendPolicy::default()
        },
    )
    .unwrap();
    (control, Egress::new(subscription))
}
fn frame(cx: &Cx, recovery: RecoveryGeneration) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::FIRST,
        FrameKind::Idr { recovery },
        CodecConfigurationGeneration::INITIAL,
        host_now(cx).unwrap().as_micros(),
        vec![7; 16],
    )
    .unwrap()
}
fn hold_final_offer(egress: &mut Egress, cx: &Cx) -> u64 {
    egress.enqueue(frame(cx, epoch().recovery)).unwrap();
    // Admit the progress announcement, but NOT the one and only payload chunk.
    egress
        .transmit(Lane::Original, |_, _, guard| {
            guard().unwrap();
            Ok::<_, ()>(Admission::Accepted)
        })
        .unwrap();
    assert!(matches!(
        egress
            .transmit(Lane::Original, |_, _, guard| {
                guard().unwrap();
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap(),
        Progress::Pending(_)
    ));
    egress.pending().unwrap().send_by_micros()
}
async fn reach(cx: &Cx, deadline: u64) {
    loop {
        let now = host_now(cx).unwrap().as_micros();
        if now >= deadline {
            return;
        }
        sleep(cx.now(), Duration::from_micros(deadline - now)).await;
    }
}

#[test]
fn idle_expiry_fences_view_and_retains_original_sender_for_replacement() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress) = setup(&cx);
        assert!(control.view_ready().unwrap());
        let expired = hold_final_offer(&mut egress, &cx);
        reach(&cx, expired).await;
        assert_eq!(egress.tick(), Err(Error::Send(SendError::OriginalExpired)));
        assert!(!egress.is_closed());
        assert!(!control.view_ready().unwrap());
        control.check().unwrap();
        assert!(egress.pending().is_none());
        assert_eq!(egress.cache_usage().bytes, 0);
        assert_eq!(egress.cache_usage().pictures, 0);
        let until = egress.next_deadline().unwrap().as_micros();
        for _ in 0..4 {
            assert_eq!(egress.tick(), Err(Error::Send(SendError::NeedsRecovery)));
            assert_eq!(egress.next_deadline().unwrap().as_micros(), until);
        }
        // Lower-level replacement models separately admitted new bindings.
        // The actual host must first complete its bound peer recovery exchange.
        let next = MediaEpoch {
            recovery: epoch().recovery.next().unwrap(),
            ..epoch()
        };
        egress
            .recover(next, MediaBindings::new(11, 12, 13, 14).unwrap())
            .unwrap();
        assert!(!control.view_ready().unwrap());
        egress.enqueue(frame(&cx, next.recovery)).unwrap();
        assert!(matches!(
            egress
                .transmit(Lane::Original, |_, _, guard| {
                    guard().unwrap();
                    Ok::<_, ()>(Admission::Accepted)
                })
                .unwrap(),
            Progress::Accepted(_)
        ));
        // Neither recovering the cache nor admitting bytes restores readiness.
        assert!(!control.view_ready().unwrap());
    });
}

#[test]
fn expired_pending_send_never_calls_transport_and_silent_wait_is_bounded() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress) = setup(&cx);
        let expired = hold_final_offer(&mut egress, &cx);
        reach(&cx, expired).await;
        assert!(matches!(
            egress.transmit::<()>(Lane::Repair, |_, _, _| panic!("expired bytes admitted")),
            Err(EgressError::Media(Error::Send(SendError::OriginalExpired)))
        ));
        assert!(!egress.is_closed());
        assert!(egress.pending().is_none());
        control.check().unwrap();
        let until = egress.next_deadline().unwrap().as_micros();
        reach(&cx, until).await;
        assert_eq!(
            egress.tick(),
            Err(Error::Send(SendError::Delivery(DeliveryError::RecoveryExpired)))
        );
        assert!(egress.is_closed());
        assert_eq!(egress.allocated_bytes(), 0);
        assert!(egress.next_deadline().is_none());
        assert!(matches!(
            egress.transmit::<()>(Lane::Original, |_, _, _| panic!("closed sender replayed")),
            Err(EgressError::Closed)
        ));
    });
}

#[test]
fn incomplete_unoffered_reference_enters_the_same_bounded_failure_state() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress) = setup(&cx);
        egress.enqueue(frame(&cx, epoch().recovery)).unwrap();
        let expired = egress.next_deadline().unwrap().as_micros();
        reach(&cx, expired).await;
        assert!(matches!(
            egress.transmit::<()>(Lane::Original, |_, _, _| panic!("broken chain emitted")),
            Err(EgressError::Media(Error::Send(SendError::OriginalExpired)))
        ));
        assert!(!egress.is_closed());
        assert!(!control.view_ready().unwrap());
        assert_eq!(egress.cache_usage().bytes, 0);
        assert!(egress.next_deadline().is_some());
    });
}

#[test]
fn claimed_expiry_of_a_useful_offer_is_not_a_recovery_bypass() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (_, mut egress) = setup(&cx);
        egress.enqueue(frame(&cx, epoch().recovery)).unwrap();
        assert!(matches!(
            egress.transmit(Lane::Original, |_, _, guard| {
                guard().unwrap();
                Ok::<_, ()>(Admission::Expired)
            }),
            Err(EgressError::Media(Error::Send(SendError::Delivery(
                DeliveryError::WrongState
            ))))
        ));
        assert!(egress.is_closed());
    });
}

#[test]
fn observation_revocation_remains_terminal_during_recovery_wait() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress) = setup(&cx);
        let expired = hold_final_offer(&mut egress, &cx);
        reach(&cx, expired).await;
        assert_eq!(egress.tick(), Err(Error::Send(SendError::OriginalExpired)));
        assert!(!egress.is_closed());
        control.revoke();
        assert!(egress.tick().is_err());
        assert!(egress.is_closed());
        assert_eq!(egress.allocated_bytes(), 0);
    });
}
