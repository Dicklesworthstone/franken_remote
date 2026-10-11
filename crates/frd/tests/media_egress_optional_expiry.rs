#![cfg(target_os = "linux")]
//! Egress/authority regression fixtures with synthetic encoded payloads.
//! These are not HEVC, actual network-admission, or impairment qualification.
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
use fr_wire::{Channel, MediaLimits, RepairRange};
use frd::{
    media::{Error, ObservationControl, Subscription, host_now},
    media_egress::{Admission, Egress, EgressError, Lane, Progress},
};
use std::time::Duration;

const HORIZON: u64 = 100_000;
fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1024, 16_384, 64).unwrap()
}
fn bindings() -> MediaBindings {
    MediaBindings::new(1, 2, 3, 4).unwrap()
}
fn frame(id: FrameId, previous: Option<FrameId>, capture: u64) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        id,
        previous.map_or(
            FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            },
            |references| FrameKind::Predicted { references },
        ),
        CodecConfigurationGeneration::INITIAL,
        capture,
        vec![7; 128],
    )
    .unwrap()
}
fn drain(egress: &mut Egress) {
    // Two records per synthetic picture: progress and a single payload record.
    for _ in 0..2 {
        assert!(matches!(
            egress
                .transmit(Lane::Original, |_, _, guard| {
                    guard().unwrap();
                    Ok::<_, ()>(Admission::Accepted)
                })
                .unwrap(),
            Progress::Accepted(_)
        ));
    }
}
fn setup(cx: &Cx) -> (ObservationControl, Egress, u64) {
    let now = host_now(cx).unwrap();
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(9),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(5_000_000),
            ticket_lifetime: HostDuration::from_micros(2_000_000),
        },
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(now).unwrap();
    authority.mark_view_ready(now).unwrap();
    authority.grant_lease(InputLeaseId::from_raw(1), now).unwrap();
    authority.mark_view_ready(now).unwrap();
    let control = ObservationControl::new(cx.clone(), authority).unwrap();
    let subscription = Subscription::new(
        control.clone(),
        limits(),
        bindings(),
        MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        SendPolicy {
            reference_horizon_micros: HORIZON,
            recovery_horizon_micros: HORIZON,
            ..SendPolicy::default()
        },
    )
    .unwrap();
    let mut egress = Egress::new(subscription);
    egress
        .enqueue(frame(FrameId::FIRST, None, now.as_micros()))
        .unwrap();
    drain(&mut egress);
    let second = FrameId::FIRST.next().unwrap();
    egress
        .enqueue(frame(second, Some(FrameId::FIRST), now.as_micros()))
        .unwrap();
    drain(&mut egress);
    let mut bytes = [0; 1024];
    let n = fr_wire::encode_repair(
        second.as_raw(),
        &[RepairRange { start: 0, end: 1 }],
        1,
        bindings().for_channel(Channel::Control),
        &limits(),
        &mut bytes,
    )
    .unwrap();
    egress.queue_repair(&bytes[..n]).unwrap();
    assert!(matches!(
        egress
            .transmit(Lane::Repair, |offer, _, guard| {
                assert_eq!(offer.expiry_error(), SendError::RepairExpired);
                guard().unwrap();
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap(),
        Progress::Pending(_)
    ));
    let until = egress.pending().unwrap().send_by_micros();
    (control, egress, until)
}
async fn reach(cx: &Cx, until: u64) {
    loop {
        let now = host_now(cx).unwrap().as_micros();
        if now >= until {
            return;
        }
        sleep(cx.now(), Duration::from_micros(until - now)).await;
    }
}
fn preparation_stall(cx: &Cx, until: u64) {
    // Fault injection inside the synchronous transport-preparation callback.
    // Production media code never blocks the authority owner this way.
    loop {
        let now = host_now(cx).unwrap().as_micros();
        if now >= until {
            return;
        }
        std::thread::sleep(Duration::from_micros(until - now));
    }
}
fn assert_usable(control: &ObservationControl, egress: &Egress) {
    control.check().unwrap();
    assert!(control.view_ready().unwrap());
    assert!(!egress.is_closed());
    assert!(egress.pending().is_none());
    assert_eq!(egress.cache_usage().bytes, 0);
    assert_eq!(egress.cache_usage().pictures, 0);
    assert_eq!(egress.allocated_bytes(), 1024);
    assert!(egress.next_deadline().is_none());
}

#[test]
fn timer_retires_a_backpressured_repair_without_fencing_the_view() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress, until) = setup(&cx);
        reach(&cx, until).await;
        egress.tick().unwrap();
        assert_usable(&control, &egress);
        // No generation replacement or IDR is needed for the next reference.
        let second = FrameId::FIRST.next().unwrap();
        egress
            .enqueue(frame(
                second.next().unwrap(),
                Some(second),
                host_now(&cx).unwrap().as_micros(),
            ))
            .unwrap();
        drain(&mut egress);
        assert!(!egress.is_closed());
    });
}

#[test]
fn changing_lane_cannot_turn_expired_repair_into_original_failure() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress, until) = setup(&cx);
        reach(&cx, until).await;
        assert_eq!(
            egress
                .transmit::<()>(Lane::Original, |_, _, _| panic!("obsolete repair sent"))
                .unwrap(),
            Progress::Idle
        );
        assert_usable(&control, &egress);
    });
}

#[test]
fn preparation_expiry_is_rechecked_and_retired_without_recovery() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress, until) = setup(&cx);
        assert_eq!(
            egress
                .transmit(Lane::Original, |_, _, guard| {
                    preparation_stall(&cx, until);
                    assert_eq!(guard(), Err(Error::Send(SendError::RepairExpired)));
                    Ok::<_, ()>(Admission::Expired)
                })
                .unwrap(),
            Progress::Idle
        );
        assert_usable(&control, &egress);
    });
}

#[test]
fn spurious_expiry_claim_cannot_discard_a_useful_repair() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (_, mut egress, _) = setup(&cx);
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
fn another_unsent_original_still_requires_recovery() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress, until) = setup(&cx);
        let second = FrameId::FIRST.next().unwrap();
        egress
            .enqueue(frame(second.next().unwrap(), Some(second), until - HORIZON))
            .unwrap();
        reach(&cx, until).await;
        assert_eq!(egress.tick(), Err(Error::Send(SendError::OriginalExpired)));
        assert!(!egress.is_closed());
        assert!(!control.view_ready().unwrap());
        assert!(egress.pending().is_none());
        assert!(egress.next_deadline().is_some());
    });
}

#[test]
fn revoked_observation_is_terminal_even_when_the_repair_has_expired() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress, until) = setup(&cx);
        reach(&cx, until).await;
        control.revoke();
        assert!(matches!(
            egress.transmit::<()>(Lane::Original, |_, _, _| panic!("revoked media sent")),
            Err(EgressError::Media(_))
        ));
        assert!(egress.is_closed());
        assert_eq!(egress.allocated_bytes(), 0);
    });
}

#[test]
fn revocation_during_preparation_is_not_hidden_by_an_expiry_claim() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (control, mut egress, until) = setup(&cx);
        assert!(matches!(
            egress.transmit(Lane::Repair, |_, _, _| {
                preparation_stall(&cx, until);
                control.revoke();
                Ok::<_, ()>(Admission::Expired)
            }),
            Err(EgressError::Media(_))
        ));
        assert!(egress.is_closed());
        assert_eq!(egress.allocated_bytes(), 0);
    });
}

#[test]
fn uncertain_transport_error_remains_terminal_after_optional_expiry() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (_, mut egress, until) = setup(&cx);
        assert!(matches!(
            egress.transmit(Lane::Repair, |_, _, guard| {
                preparation_stall(&cx, until);
                assert_eq!(guard(), Err(Error::Send(SendError::RepairExpired)));
                Err::<Admission, _>("uncertain transport admission")
            }),
            Err(EgressError::Transport("uncertain transport admission"))
        ));
        assert!(egress.is_closed());
        assert_eq!(egress.allocated_bytes(), 0);
    });
}
