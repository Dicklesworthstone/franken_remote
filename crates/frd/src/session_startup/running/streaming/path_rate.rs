//! One path sample for ordinary service and every bounded recovery wait.
//!
//! A failed reference suspends the picture, not the original connection's
//! congestion evidence. Continuing to observe that path can justify a smaller
//! recovery IDR; the same owner, rate floor/ceiling and dwell still apply. This
//! function grants no capture credit, freshness, or authority. Call only after
//! the session's current permission and original deadline checks.
use crate::media::shared_publisher::path_queue::PathQueue;
use fr_media::bitrate::{Controller, Error};
use fr_transport::quic::PathLoad;

pub(super) fn observe(
    path: &mut PathQueue,
    bitrate: Option<&mut Controller>,
    load: Option<PathLoad>,
    now: u64,
) -> Result<(), Error> {
    path.observe(load, now);
    if let Some(bitrate) = bitrate {
        bitrate.observe_path_evidence(now, path.evidence(now))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_media::bitrate::{PathEvidence, Reason};

    const INITIAL: u32 = 2_000_000;

    fn load(rtt: u64, bytes: u64, packets: u64) -> PathLoad {
        PathLoad {
            latest_rtt_us: rtt,
            smoothed_rtt_us: rtt + packets % 2,
            rtt_variation_us: 100,
            bytes_in_flight: bytes,
            packets_acked: packets,
            busy_packets_acked: 0,
            busy_acked_at_us: None,
        }
    }

    fn congested(at: u64) -> PathLoad {
        load(200_000, 24_000, at / 10_000)
    }

    #[test]
    fn recovery_turns_finish_pressure_dwell_on_the_original_rate_owner() {
        let mut path = PathQueue::default();
        let mut bitrate = Controller::new(INITIAL).unwrap();
        // Ordinary service sees a healthy route, then a queue just before its
        // reference chain fails. Neither sample alone admits a rate reduction.
        observe(
            &mut path,
            Some(&mut bitrate),
            Some(load(5_000, 12_000, 0)),
            0,
        )
        .unwrap();
        observe(
            &mut path,
            Some(&mut bitrate),
            Some(congested(10_000)),
            10_000,
        )
        .unwrap();
        assert!(bitrate.pending().is_none());

        // These are the same bounded samples now consumed by failed-chain,
        // attachment and decoder waiting turns. Previously those turns never
        // called this owner, leaving the recovery capture at INITIAL.
        for at in (20_000..=200_000).step_by(10_000) {
            observe(&mut path, Some(&mut bitrate), Some(congested(at)), at).unwrap();
            assert!(bitrate.pending().is_none());
        }
        observe(
            &mut path,
            Some(&mut bitrate),
            Some(congested(210_000)),
            210_000,
        )
        .unwrap();
        let cut = bitrate.pending().unwrap();
        assert_eq!(cut.reason(), Reason::PathQueue);
        assert!(cut.bitrate() >= bitrate.minimum_bitrate());
        assert!(cut.bitrate() < INITIAL);
        assert_eq!(
            bitrate.applied_bitrate(),
            INITIAL,
            "proposal is not native output"
        );
        let capture = bitrate.begin_capture().unwrap();
        assert_eq!(capture.bitrate(), cut.bitrate());
        assert_eq!(bitrate.begin_capture(), Err(Error::CaptureInFlight));
        bitrate
            .complete_capture(capture, cut.bitrate(), true, 220_000)
            .unwrap();
        assert_eq!(bitrate.applied_bitrate(), cut.bitrate());
        assert!(bitrate.pending().is_none());
        // Recovery does not reset the original cooldown or permit another cut.
        for at in (230_000..=500_000).step_by(10_000) {
            observe(&mut path, Some(&mut bitrate), Some(congested(at)), at).unwrap();
            assert!(bitrate.pending().is_none());
        }
    }

    #[test]
    fn a_scheduling_gap_cannot_complete_recovery_pressure_dwell() {
        let mut path = PathQueue::default();
        let mut bitrate = Controller::new(INITIAL).unwrap();
        observe(
            &mut path,
            Some(&mut bitrate),
            Some(load(5_000, 12_000, 0)),
            0,
        )
        .unwrap();
        observe(
            &mut path,
            Some(&mut bitrate),
            Some(congested(10_000)),
            10_000,
        )
        .unwrap();
        for at in (300_000..500_000).step_by(10_000) {
            observe(&mut path, Some(&mut bitrate), Some(congested(at)), at).unwrap();
            assert!(bitrate.pending().is_none());
        }
        observe(
            &mut path,
            Some(&mut bitrate),
            Some(congested(500_000)),
            500_000,
        )
        .unwrap();
        assert_eq!(bitrate.pending().unwrap().reason(), Reason::PathQueue);
    }

    fn reduced() -> Controller {
        let mut bitrate = Controller::new(INITIAL).unwrap();
        for at in (0..=200_000).step_by(20_000) {
            bitrate
                .observe_path_evidence(
                    at,
                    PathEvidence::Queued {
                        delivered_bits_per_second: 0,
                    },
                )
                .unwrap();
        }
        let capture = bitrate.begin_capture().unwrap();
        bitrate
            .complete_capture(capture, capture.bitrate(), true, 200_000)
            .unwrap();
        bitrate
    }

    #[test]
    fn idle_unknown_or_cadence_only_recovery_cannot_create_a_probe() {
        for unknown in [false, true] {
            let mut path = PathQueue::default();
            let mut bitrate = reduced();
            let applied = bitrate.applied_bitrate();
            for at in (210_000..=6_000_000).step_by(10_000) {
                let sample = (!unknown).then(|| load(5_000, 0, at / 10_000));
                observe(&mut path, Some(&mut bitrate), sample, at).unwrap();
                assert!(bitrate.pending().is_none());
            }
            assert_eq!(bitrate.applied_bitrate(), applied);
        }
        let mut path = PathQueue::default();
        observe(&mut path, None, Some(load(5_000, 12_000, 0)), 0).unwrap();
        observe(&mut path, None, Some(congested(10_000)), 10_000).unwrap();
        assert!(
            path.queued(),
            "cadence-only callers still retain their path gate"
        );
    }

    #[test]
    fn recovery_withdraws_an_unsubmitted_probe_before_requesting_an_idr() {
        for sample in [None, Some(load(5_000, 0, 10)), Some(congested(4_210_000))] {
            let mut bitrate = reduced();
            for at in (200_000..=4_200_000).step_by(20_000) {
                bitrate
                    .observe_path_evidence(at, PathEvidence::Clear)
                    .unwrap();
            }
            assert_eq!(bitrate.pending().unwrap().reason(), Reason::Headroom);
            let applied = bitrate.applied_bitrate();
            let mut path = PathQueue::default();
            observe(&mut path, Some(&mut bitrate), sample, 4_210_000).unwrap();
            assert!(bitrate.pending().is_none());
            assert_eq!(bitrate.begin_capture().unwrap().bitrate(), applied);
        }
    }

    #[test]
    fn pressure_observed_during_native_work_does_not_replace_its_original_token() {
        let mut path = PathQueue::default();
        let mut bitrate = Controller::new(INITIAL).unwrap();
        let capture = bitrate.begin_capture().unwrap();
        observe(
            &mut path,
            Some(&mut bitrate),
            Some(load(5_000, 12_000, 0)),
            0,
        )
        .unwrap();
        for at in (10_000..=210_000).step_by(10_000) {
            observe(&mut path, Some(&mut bitrate), Some(congested(at)), at).unwrap();
        }
        let cut = bitrate.pending().unwrap();
        assert_eq!(bitrate.begin_capture(), Err(Error::CaptureInFlight));
        bitrate
            .complete_capture(capture, INITIAL, true, 220_000)
            .unwrap();
        assert_eq!(bitrate.applied_bitrate(), INITIAL);
        assert_eq!(bitrate.pending(), Some(cut));
        assert_eq!(bitrate.begin_capture().unwrap().bitrate(), cut.bitrate());
    }
}
