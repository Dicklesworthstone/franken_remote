#![cfg(target_os = "linux")]
//! Real subscription/packet owners with a virtual clock and recording transport
//! admission. No UDP, decoder, compositor or physical latency is modeled here.
use asupersync::{
    cx::Cx,
    runtime::RuntimeBuilder,
    time::{TimerDriverHandle, VirtualClock},
};
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{CodecConfigurationGeneration, RecoveryGeneration, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::{MediaBindings, MediaEpoch, PacketOffer, SendError, SendPolicy},
};
use fr_wire::{Channel, MediaLimits, RepairRange, encode_repair};
use frd::{
    media::{Error, ObservationControl, Subscription, host_now},
    media_egress::{Admission, Egress, EgressError, Lane, Progress},
};
use std::sync::Arc;

fn run(test: impl FnOnce(&Cx, &VirtualClock)) {
    let clock = Arc::new(VirtualClock::new());
    let runtime = RuntimeBuilder::current_thread()
        .with_timer_driver(TimerDriverHandle::with_virtual_clock(Arc::clone(&clock)))
        .build()
        .unwrap();
    runtime.block_on(async {
        let cx = Cx::current().unwrap();
        test(&cx, &clock);
    });
}
fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1024, 16_384, 64).unwrap()
}
fn bindings() -> MediaBindings {
    MediaBindings::new(1, 2, 3, 4).unwrap()
}
fn picture(cx: &Cx, frame: u64, count: u32) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::from_raw(frame),
        FrameKind::Predicted {
            references: FrameId::from_raw(frame - 1),
        },
        CodecConfigurationGeneration::INITIAL,
        host_now(cx).unwrap().as_micros(),
        vec![17; usize::try_from(count * limits().fragment_stride()).unwrap()],
    )
    .unwrap()
}
fn first_picture() -> u64 {
    FrameId::FIRST.as_raw() + 1
}
fn started(cx: &Cx) -> (Egress, ObservationControl) {
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(9),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(host_now(cx).unwrap())
        .unwrap();
    let control = ObservationControl::new(cx.clone(), authority).unwrap();
    let subscription = Subscription::new(
        control.clone(),
        limits(),
        bindings(),
        MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        SendPolicy::default(),
    )
    .unwrap();
    let mut egress = Egress::new(subscription);
    egress
        .enqueue(
            EncodedAccessUnit::new(
                &ProtocolLimits::ABSOLUTE,
                FrameId::FIRST,
                FrameKind::Idr {
                    recovery: RecoveryGeneration::INITIAL,
                },
                CodecConfigurationGeneration::INITIAL,
                host_now(cx).unwrap().as_micros(),
                vec![7; 16],
            )
            .unwrap(),
        )
        .unwrap();
    loop {
        let progress = egress
            .transmit(Lane::Original, |_, _, guard| {
                guard().unwrap();
                Ok::<_, ()>(Admission::Accepted)
            })
            .unwrap();
        if progress == Progress::Idle {
            break;
        }
    }
    (egress, control)
}
fn packet(egress: &mut Egress, lane: Option<Lane>) -> (PacketOffer, Vec<u8>) {
    let mut saved = None;
    let send = |offer: &PacketOffer, bytes: &[u8], guard: &mut dyn FnMut() -> Result<(), Error>| {
        guard().unwrap();
        assert!(
            saved.is_none(),
            "more than one transport callback in a turn"
        );
        saved = Some((offer.clone(), bytes.to_vec()));
        Ok::<_, ()>(Admission::Accepted)
    };
    let progress = match lane {
        Some(lane) => egress.transmit(lane, send),
        None => egress.transmit_interleaved(send),
    }
    .unwrap();
    let (offer, bytes) = saved.expect("one eligible packet");
    assert_eq!(progress, Progress::Accepted(offer.clone()));
    (offer, bytes)
}
fn announce(cx: &Cx, egress: &mut Egress, count: u32) {
    egress.enqueue(picture(cx, first_picture(), count)).unwrap();
    let (offer, _) = packet(egress, Some(Lane::Original));
    assert_eq!(offer.channel(), Channel::MediaConfig);
}
fn request(egress: &mut Egress, start: u32, end: u32, total: u32) {
    let mut bytes = [0; 1024];
    let count = encode_repair(
        first_picture(),
        &[RepairRange { start, end }],
        total,
        bindings().for_channel(Channel::Control),
        &limits(),
        &mut bytes,
    )
    .unwrap();
    egress.queue_repair(&bytes[..count]).unwrap();
}
fn is_repair(offer: &PacketOffer) -> bool {
    offer.expiry_error() == SendError::RepairExpired
}

#[test]
fn a_busy_original_tail_cannot_starve_an_eligible_prefix_repair() {
    run(|cx, _| {
        let (mut egress, _) = started(cx);
        announce(cx, &mut egress, 8);
        let mut originals = Vec::new();
        for _ in 0..3 {
            originals.push(packet(&mut egress, Some(Lane::Original)).1);
        }
        request(&mut egress, 0, 3, 8);
        let usage = egress.cache_usage();
        for original in originals {
            let (repair, bytes) = packet(&mut egress, None);
            assert!(is_repair(&repair));
            assert_eq!(bytes, original);
            let (tail, _) = packet(&mut egress, None);
            assert!(!is_repair(&tail));
            assert_eq!(tail.channel(), Channel::Video);
            assert_eq!(tail.send_by_micros(), repair.send_by_micros());
        }
        assert_eq!(egress.cache_usage(), usage);
        assert_eq!(egress.allocated_bytes(), 1024);
    });
}

#[test]
fn waiting_for_unissued_requested_fragments_still_advances_originals() {
    run(|cx, _| {
        let (mut egress, _) = started(cx);
        announce(cx, &mut egress, 8);
        let mut originals = vec![packet(&mut egress, Some(Lane::Original)).1];
        // A tail request names fragments 6 and 7 before either original exists.
        request(&mut egress, 6, 8, 8);
        // Every turn still sends something: the waiting tail never stalls the
        // originals. Each requested fragment is gated by its OWN original
        // (6506e20), so repair 6 may precede original 7, but no repair can
        // precede, or stand in for, its original.
        let mut repaired = Vec::new();
        for _ in 0..9 {
            let (offer, bytes) = packet(&mut egress, None);
            if is_repair(&offer) {
                let index = originals.iter().position(|o| *o == bytes);
                repaired.push(index.expect("a repair of an unissued original"));
            } else {
                assert_eq!(offer.channel(), Channel::Video);
                originals.push(bytes);
            }
        }
        assert_eq!(originals.len(), 8);
        assert_eq!(repaired, [6, 7]);
        assert_eq!(
            egress
                .transmit_interleaved::<()>(|_, _, _| panic!("empty lanes submitted"))
                .unwrap(),
            Progress::Idle
        );
    });
}

#[test]
fn a_pending_repair_keeps_its_bytes_and_next_original_turn() {
    run(|cx, _| {
        let (mut egress, _) = started(cx);
        announce(cx, &mut egress, 8);
        packet(&mut egress, Some(Lane::Original));
        packet(&mut egress, Some(Lane::Original));
        request(&mut egress, 0, 2, 8);
        let mut saved = None;
        let pending = egress
            .transmit_interleaved(|offer, bytes, guard| {
                guard().unwrap();
                saved = Some((offer.clone(), bytes.to_vec()));
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        let (offer, bytes) = saved.unwrap();
        assert!(is_repair(&offer));
        assert_eq!(pending, Progress::Pending(offer.clone()));
        for _ in 0..9 {
            let again = egress
                .transmit(Lane::Original, |again, body, guard| {
                    guard().unwrap();
                    assert_eq!(again, &offer);
                    assert_eq!(body, bytes);
                    Ok::<_, ()>(Admission::Backpressure)
                })
                .unwrap();
            assert_eq!(again, pending);
        }
        assert_eq!(packet(&mut egress, None), (offer, bytes));
        assert!(!is_repair(&packet(&mut egress, None).0));
        assert!(is_repair(&packet(&mut egress, None).0));
    });
}

#[test]
fn an_original_prepared_before_a_request_still_wins_unchanged() {
    run(|cx, _| {
        let (mut egress, _) = started(cx);
        announce(cx, &mut egress, 8);
        packet(&mut egress, Some(Lane::Original));
        let mut saved = None;
        egress
            .transmit(Lane::Original, |offer, bytes, guard| {
                guard().unwrap();
                saved = Some((offer.clone(), bytes.to_vec()));
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        request(&mut egress, 0, 1, 8);
        assert_eq!(packet(&mut egress, None), saved.unwrap());
        assert!(is_repair(&packet(&mut egress, None).0));
    });
}

#[test]
fn explicit_original_selection_does_not_open_the_repair_lane() {
    run(|cx, _| {
        let (mut egress, _) = started(cx);
        announce(cx, &mut egress, 4);
        for _ in 0..4 {
            packet(&mut egress, Some(Lane::Original));
        }
        request(&mut egress, 0, 1, 4);
        assert_eq!(
            egress
                .transmit::<()>(Lane::Original, |_, _, _| panic!("startup sent repair"))
                .unwrap(),
            Progress::Idle
        );
        assert!(is_repair(&packet(&mut egress, None).0));
    });
}

#[test]
fn optional_expiry_yields_without_sending_a_fresh_successor_in_the_same_call() {
    run(|cx, clock| {
        let (mut egress, _) = started(cx);
        announce(cx, &mut egress, 4);
        for _ in 0..4 {
            packet(&mut egress, Some(Lane::Original));
        }
        request(&mut egress, 0, 1, 4);
        egress
            .transmit_interleaved(|_, _, guard| {
                guard().unwrap();
                Ok::<_, ()>(Admission::Backpressure)
            })
            .unwrap();
        let until = egress.pending().unwrap().send_by_micros();
        clock.advance(200_000_000);
        egress.enqueue(picture(cx, first_picture() + 1, 4)).unwrap();
        let now = host_now(cx).unwrap().as_micros();
        clock.advance((until - now) * 1000);
        assert_eq!(
            egress
                .transmit_interleaved::<()>(|_, _, _| panic!("expired turn sent bytes"))
                .unwrap(),
            Progress::Idle
        );
        assert!(!egress.is_closed());
        assert!(egress.pending().is_none());
        let (successor, _) = packet(&mut egress, None);
        assert_eq!(successor.frame(), first_picture() + 1);
        assert_eq!(successor.channel(), Channel::MediaConfig);
    });
}

#[test]
fn original_expiry_and_revocation_are_not_hidden_by_lane_fallback() {
    for revoke in [false, true] {
        run(|cx, clock| {
            let (mut egress, control) = started(cx);
            announce(cx, &mut egress, 8);
            packet(&mut egress, Some(Lane::Original));
            request(&mut egress, 0, 1, 8);
            if revoke {
                control.revoke();
            } else {
                clock.advance(250_000_000);
            }
            assert!(matches!(
                egress.transmit_interleaved::<()>(|_, _, _| panic!("invalid authority sent")),
                Err(EgressError::Media(_))
            ));
        });
    }
}

#[test]
fn a_transport_failure_is_not_retried_on_the_other_lane() {
    run(|cx, _| {
        let (mut egress, _) = started(cx);
        announce(cx, &mut egress, 8);
        packet(&mut egress, Some(Lane::Original));
        request(&mut egress, 0, 1, 8);
        let mut calls = 0;
        let result = egress.transmit_interleaved(|_, _, guard| {
            guard().unwrap();
            calls += 1;
            Err::<Admission, _>(())
        });
        assert!(matches!(result, Err(EgressError::Transport(()))));
        assert_eq!(calls, 1);
        assert!(egress.is_closed());
        assert!(matches!(
            egress.transmit_interleaved::<()>(|_, _, _| panic!("uncertain write retried")),
            Err(EgressError::Closed)
        ));
    });
}
