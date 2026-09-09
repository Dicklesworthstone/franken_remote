#![cfg(target_os = "linux")]
use asupersync::{cx::Cx, runtime::RuntimeBuilder, time::sleep, types::Budget};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    limits::ProtocolLimits,
    time::HostDuration,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::*,
};
use fr_wire::{Channel, MediaLimits};
use frd::{
    media::{ObservationControl, Subscription, host_now},
    media_egress::*,
};
use std::time::Duration;
fn gate(cx: Cx, lifetime: u64) -> ObservationControl {
    let mut a = SessionAuthority::new(
        RemoteSessionId::from_raw(9),
        AuthorityPolicy {
            authorization_lifetime: HostDuration::from_micros(lifetime),
            ticket_lifetime: HostDuration::from_micros(lifetime / 2),
        },
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(host_now(&cx).unwrap()).unwrap();
    ObservationControl::new(cx, a).unwrap()
}
fn setup(c: ObservationControl) -> Egress {
    Egress::new(Subscription::new(c, limits(), bindings(), epoch(), SendPolicy::default()).unwrap())
}
fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1024, 16384, 64).unwrap()
}
fn bindings() -> MediaBindings {
    MediaBindings::new(1, 2, 3, 4).unwrap()
}
fn epoch() -> MediaEpoch {
    MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn frame(cx: &Cx) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::FIRST,
        FrameKind::Idr {
            recovery: RecoveryGeneration::INITIAL,
        },
        CodecConfigurationGeneration::INITIAL,
        host_now(cx).unwrap().as_micros(),
        vec![7; 5000],
    )
    .unwrap()
}
#[test]
fn backpressure_preserves_every_recovery_chunk_and_original_deadline() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let mut egress = setup(gate(cx.clone(), 3_000_000));
        egress.enqueue(frame(&cx)).unwrap();
        let mut receiver = ReceivePipeline::new(
            ReceiveConfig {
                limits: limits(),
                bindings: bindings(),
                epoch: epoch(),
                policy: ReceivePolicy::default(),
            },
            MediaBudget::new(limits().protocol()).unwrap(),
        )
        .unwrap();
        receiver
            .decoder_configured(host_now(&cx).unwrap().as_micros())
            .unwrap();
        let mut accepted = 0;
        loop {
            let mut saved = None;
            let outcome = egress
                .transmit(Lane::Original, |offer, bytes, guard| {
                    guard().unwrap();
                    saved = Some((offer.clone(), bytes.to_vec()));
                    Ok::<_, ()>(Admission::Backpressure)
                })
                .unwrap();
            if outcome == Progress::Idle {
                break;
            }
            let (offer, bytes) = saved.unwrap();
            for _ in 0..3 {
                // Changing selected lane cannot bypass the already prepared record.
                egress
                    .transmit(Lane::Repair, |again, body, guard| {
                        guard().unwrap();
                        assert_eq!(*again, offer);
                        assert_eq!(body, bytes);
                        Ok::<_, ()>(Admission::Backpressure)
                    })
                    .unwrap();
            }
            let result = egress
                .transmit(Lane::Original, |again, body, guard| {
                    guard().unwrap();
                    assert_eq!(*again, offer);
                    assert_eq!(body, bytes);
                    receiver
                        .receive(offer.channel(), body, host_now(&cx).unwrap().as_micros())
                        .unwrap();
                    Ok::<_, ()>(Admission::Accepted)
                })
                .unwrap();
            assert_eq!(result, Progress::Accepted(offer));
            accepted += 1;
            assert!(egress.pending().is_none());
            assert_eq!(egress.allocated_bytes(), 1024);
        }
        assert!(accepted > 4);
        let recovered = receiver
            .take_decodable(host_now(&cx).unwrap().as_micros())
            .unwrap()
            .unwrap();
        assert_eq!(recovered.bytes(), vec![7; 5000]);
    });
}
#[test]
fn revoke_stops_pending_packet_without_invoking_transport() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let control = gate(cx.clone(), 3_000_000);
        let mut egress = setup(control.clone());
        egress.enqueue(frame(&cx)).unwrap();
        egress
            .transmit(Lane::Original, |_, _, _| {
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        control.revoke();
        assert!(matches!(
            egress.transmit::<()>(Lane::Original, |_, _, _| panic!("revoked record submitted")),
            Err(EgressError::Media(_))
        ));
        assert!(egress.is_closed());
        assert_eq!(egress.allocated_bytes(), 0);
    });
}
#[test]
fn expired_pending_packet_cannot_obtain_a_fresh_queue_lifetime() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let control = gate(cx.clone(), 15_000);
        let mut egress = setup(control);
        egress.enqueue(frame(&cx)).unwrap();
        egress
            .transmit(Lane::Original, |_, _, _| {
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        let old = egress.pending().unwrap().clone();
        sleep(cx.now(), Duration::from_millis(25)).await;
        assert!(matches!(
            egress.transmit::<()>(Lane::Original, |_, _, _| panic!("expired record submitted")),
            Err(EgressError::Media(_))
        ));
        assert!(old.send_by_micros() > 0);
        assert!(egress.is_closed());
    });
}
#[test]
fn guard_rechecks_revocation_inside_native_preparation_and_error_is_terminal() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let control = gate(cx.clone(), 3_000_000);
        let mut egress = setup(control.clone());
        egress.enqueue(frame(&cx)).unwrap();
        let result = egress.transmit(Lane::Original, |_, _, guard| {
            control.revoke();
            assert!(guard().is_err());
            Err::<Admission, _>("private foreign error")
        });
        assert!(matches!(result, Err(EgressError::Transport(_))));
        assert_eq!(format!("{:?}", result.unwrap_err()), "Transport");
        assert!(matches!(
            egress.transmit::<()>(Lane::Original, |_, _, _| panic!("uncertain effect retried")),
            Err(EgressError::Closed)
        ));
    });
}
#[test]
fn abandoning_subscription_does_not_revoke_shared_observation_or_other_viewers() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let control = gate(cx.clone(), 3_000_000);
        let mut first = setup(control.clone());
        let mut second = setup(control.clone());
        first.enqueue(frame(&cx)).unwrap();
        second.enqueue(frame(&cx)).unwrap();
        first
            .transmit(Lane::Original, |_, _, _| {
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        first.close();
        assert!(control.check().is_ok());
        assert!(matches!(
            second
                .transmit(Lane::Original, |offer, _, guard| {
                    assert_eq!(offer.channel(), Channel::MediaConfig);
                    guard().unwrap();
                    Ok::<_, ()>(Admission::Accepted)
                })
                .unwrap(),
            Progress::Accepted(_)
        ));
    });
}

#[test]
fn idle_tick_releases_expired_pending_work_without_another_send() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let mut egress = setup(gate(cx.clone(), 10_000));
        egress.enqueue(frame(&cx)).unwrap();
        egress
            .transmit(Lane::Original, |_, _, _| {
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        assert_eq!(egress.cache_usage().pictures, 1);
        assert!(egress.next_deadline().is_some());
        sleep(cx.now(), Duration::from_millis(20)).await;
        assert!(egress.tick().is_err());
        assert!(egress.is_closed());
        assert!(egress.pending().is_none());
        assert!(egress.next_deadline().is_none());
        assert_eq!(egress.allocated_bytes(), 0);
        assert_eq!(egress.cache_usage().bytes, 0);
        assert_eq!(egress.cache_usage().pictures, 0);
    });
}
#[test]
fn packet_expiry_during_idle_tick_is_not_confused_with_live_authority() {
    let runtime = RuntimeBuilder::new().worker_threads(1).build().unwrap();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let control = gate(cx.clone(), 3_000_000);
        let subscription = Subscription::new(
            control.clone(),
            limits(),
            bindings(),
            epoch(),
            SendPolicy {
                recovery_horizon_micros: 10_000,
                ..SendPolicy::default()
            },
        )
        .unwrap();
        let mut egress = Egress::new(subscription);
        egress.enqueue(frame(&cx)).unwrap();
        egress
            .transmit(Lane::Original, |_, _, _| {
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        sleep(cx.now(), Duration::from_millis(20)).await;
        assert!(control.check().is_ok());
        assert!(egress.tick().is_err());
        assert!(egress.is_closed());
        assert_eq!(egress.cache_usage().bytes, 0);
        assert!(control.check().is_ok());
    });
}
