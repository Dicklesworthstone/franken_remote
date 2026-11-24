use super::*;

fn rescue() -> CapturePacing {
    let mut pacing = pacing();
    pacing.controller = Controller::for_receiver_overload(pacing.controller.policy()).unwrap();
    pacing
}

fn load(retained_pictures: u32) -> Load {
    Load {
        retained_bytes: u64::from(retained_pictures) * 1024,
        retained_pictures,
        decoding: false,
        work_us: Some(1_000),
    }
}

fn peer(now: u64, load: Load) -> (Option<(u64, Availability)>, ReceiverEvidence) {
    (
        Some((now, Availability::Ready)),
        ReceiverEvidence::Measured(load),
    )
}

#[test]
fn rescue_keeps_default_path_only_bitrate_despite_local_send_and_credit_pressure() {
    let mut pacing = rescue();
    pacing.enable_bitrate(2_000_000).unwrap();
    for now in (0..=5_000_000).step_by(20_000) {
        let evidence = PeerEvidence {
            send: Availability::Blocked,
            receiver: ReceiverEvidence::Unknown,
            reset_receiver: false,
        };
        assert_eq!(pacing.sample(now, false, evidence).unwrap(), 50_000);
        assert_eq!(pacing.bitrate.as_ref().unwrap().pending(), None);
    }
    let rate = pacing.bitrate.as_mut().unwrap();
    for now in (5_020_000..=5_220_000).step_by(20_000) {
        rate.observe_path_evidence(
            now,
            bitrate::PathEvidence::Queued {
                delivered_bits_per_second: 500_000,
            },
        )
        .unwrap();
    }
    assert_eq!(rate.pending().unwrap().reason(), bitrate::Reason::PathQueue);
}

#[test]
fn a_whole_healthy_measured_sibling_outvotes_pressure_and_legacy_cannot_mask_recovery() {
    for legacy_first in [false, true] {
        let mut pacing = rescue();
        for now in (10_000u64..=1_010_000).step_by(10_000) {
            let evidence = receiver_evidence(
                now,
                pacing.interval_us(),
                [peer(now, load(2)), peer(now, load(1))].into_iter(),
                true,
            );
            assert_eq!(pacing.sample(now, true, evidence).unwrap(), 50_000);
        }
        for now in (1_020_000..=2_020_000).step_by(10_000) {
            let evidence = receiver_evidence(
                now,
                pacing.interval_us(),
                [peer(now, load(2))].into_iter(),
                true,
            );
            pacing.sample(now, true, evidence).unwrap();
        }
        assert_eq!(pacing.interval_us(), 100_000);
        for now in (2_030_000u64..=16_000_000).step_by(10_000) {
            if now.is_multiple_of(50_000) {
                complete(&mut pacing, now, true);
            }
            let legacy = (
                Some((now, Availability::Ready)),
                ReceiverEvidence::Unobserved,
            );
            let measured = peer(now, load(1));
            let peers = if legacy_first {
                [legacy, measured]
            } else {
                [measured, legacy]
            };
            let evidence = receiver_evidence(now, pacing.interval_us(), peers.into_iter(), true);
            assert!(matches!(evidence.receiver, ReceiverEvidence::Measured(_)));
            pacing.sample(now, true, evidence).unwrap();
        }
        assert_eq!(pacing.interval_us(), 50_000);
        assert!(pacing.controller.decisions().any(|report| {
            report.reason == pacing::Reason::HeadroomProbe
                && matches!(report.receiver, ReceiverEvidence::Measured(_))
        }));
        let legacy = (
            Some((16_010_000, Availability::Ready)),
            ReceiverEvidence::Unobserved,
        );
        assert_eq!(
            peer_evidence(
                16_010_000,
                50_000,
                [legacy, peer(16_010_000, load(1))].into_iter(),
            )
            .receiver,
            ReceiverEvidence::Unobserved,
            "explicit adaptive aggregation retains its prior selection order"
        );
    }
}
