use super::*;
use crate::media::shared_publisher::MAX_SUBSCRIBERS;

const INITIAL: u32 = 8_000_000;

fn enabled() -> CapturePacing {
    let mut pacing = pacing();
    pacing.enable_bitrate(INITIAL).unwrap();
    pacing
}
fn policy(pacing: &CapturePacing) -> &bitrate::Controller {
    pacing.bitrate.as_ref().unwrap()
}
fn cohort() -> [Option<Binding>; MAX_SUBSCRIBERS] {
    let mut views = [None; MAX_SUBSCRIBERS];
    views[0] = Some(binding());
    views
}
fn pressure(pacing: &mut CapturePacing) {
    for now in (0..=100_000).step_by(10_000) {
        let blocked = (
            Some((now, Availability::Blocked)),
            ReceiverEvidence::Unobserved,
        );
        let evidence = peer_evidence(now, pacing.interval_us(), [blocked, blocked].into_iter());
        pacing.sample(now, true, evidence).unwrap();
    }
    assert_eq!(policy(pacing).pending().unwrap().bitrate(), INITIAL / 2);
}
fn completed(pacing: &mut CapturePacing, start: u64, actual: u32, idr: bool) {
    pacing
        .complete_bitrate(start + 1000, Some(actual), idr)
        .unwrap();
    pacing.finish(start + 1000, start, idr).unwrap();
}

#[test]
fn healthy_or_unknown_shared_send_evidence_vetoes_a_single_slow_viewers_rate_cut() {
    for other in [Availability::Ready, Availability::Unknown] {
        let mut pacing = enabled();
        for now in (0..=400_000).step_by(10_000) {
            let blocked = (
                Some((now, Availability::Blocked)),
                ReceiverEvidence::Unobserved,
            );
            let sibling = (Some((now, other)), ReceiverEvidence::Unobserved);
            let evidence = peer_evidence(now, pacing.interval_us(), [blocked, sibling].into_iter());
            assert_ne!(evidence.send, Availability::Blocked);
            pacing.sample(now, true, evidence).unwrap();
        }
        assert_eq!(policy(&pacing).applied_bitrate(), INITIAL);
        assert!(policy(&pacing).pending().is_none());
    }
}

#[test]
fn shared_rate_changes_only_after_the_original_native_capture_confirms_an_idr() {
    let mut pacing = enabled();
    pressure(&mut pacing);
    let decision = policy(&pacing).pending().unwrap();
    pacing.start(110_000).unwrap();
    assert_eq!(pacing.capture_bitrate(), Some(INITIAL / 2));
    assert_eq!(pacing.start(110_001), Err(Error::InvalidBudget));
    completed(&mut pacing, 110_000, INITIAL, false);
    assert_eq!(policy(&pacing).pending(), Some(decision));
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL);
    pacing.start(120_000).unwrap();
    completed(&mut pacing, 120_000, INITIAL / 2, true);
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL / 2);
    assert!(policy(&pacing).pending().is_none());
}

#[test]
fn membership_reset_cancels_unsent_intent_and_explicitly_cancels_native_deferred_intent() {
    for submitted in [false, true] {
        let mut pacing = enabled();
        let mut views = cohort();
        pacing.observe_receiver_views(&views).unwrap();
        pressure(&mut pacing);
        if submitted {
            pacing.start(110_000).unwrap();
            completed(&mut pacing, 110_000, INITIAL, false);
        }
        views[0].as_mut().unwrap().recovery = binding().recovery.next().unwrap();
        pacing.observe_receiver_views(&views).unwrap();
        if submitted {
            let reset = policy(&pacing).pending().unwrap();
            assert_eq!(reset.reason(), bitrate::Reason::EvidenceReset);
            assert_eq!(reset.bitrate(), INITIAL);
        } else {
            assert!(policy(&pacing).pending().is_none());
        }
        pacing.start(120_000).unwrap();
        assert_eq!(pacing.capture_bitrate(), Some(INITIAL));
        completed(&mut pacing, 120_000, INITIAL, false);
        assert!(policy(&pacing).pending().is_none());
    }
}

#[test]
fn membership_reset_cannot_erase_an_already_running_native_rate_change() {
    let mut pacing = enabled();
    let mut views = cohort();
    pacing.observe_receiver_views(&views).unwrap();
    pressure(&mut pacing);
    pacing.start(110_000).unwrap();
    let original = pacing.bitrate_capture;
    views[1] = views[0].take();
    pacing.observe_receiver_views(&views).unwrap();
    assert_eq!(pacing.bitrate_capture, original);
    completed(&mut pacing, 110_000, INITIAL / 2, true);
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL / 2);
    let reset = policy(&pacing).pending().unwrap();
    assert_eq!(reset.reason(), bitrate::Reason::EvidenceReset);
    assert_eq!(reset.bitrate(), INITIAL);
    pacing.start(120_000).unwrap();
    assert_eq!(pacing.capture_bitrate(), Some(INITIAL));
    completed(&mut pacing, 120_000, INITIAL / 2, false);
    assert_eq!(policy(&pacing).pending(), Some(reset));
    pacing.start(2_120_000).unwrap();
    completed(&mut pacing, 2_120_000, INITIAL, true);
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL);
    assert!(policy(&pacing).pending().is_none());
}

#[test]
fn unchanged_cohort_preserves_a_deferred_request_and_removal_clears_it() {
    let mut pacing = enabled();
    let views = cohort();
    pacing.observe_receiver_views(&views).unwrap();
    pressure(&mut pacing);
    pacing.start(110_000).unwrap();
    completed(&mut pacing, 110_000, INITIAL, false);
    let original = policy(&pacing).pending();
    pacing.observe_receiver_views(&views).unwrap();
    assert_eq!(policy(&pacing).pending(), original);
    pacing
        .observe_receiver_views(&[None; MAX_SUBSCRIBERS])
        .unwrap();
    assert_eq!(
        policy(&pacing).pending().unwrap().reason(),
        bitrate::Reason::EvidenceReset
    );
}

#[test]
fn missing_wrong_or_non_idr_rate_results_cannot_finish_shared_capture_accounting() {
    for (actual, idr) in [
        (None, false),
        (Some(3_000_000), true),
        (Some(INITIAL / 2), false),
    ] {
        let mut pacing = enabled();
        pressure(&mut pacing);
        pacing.start(110_000).unwrap();
        assert!(pacing.complete_bitrate(111_000, actual, idr).is_err());
        assert_eq!(policy(&pacing).applied_bitrate(), INITIAL);
        assert_eq!(
            pacing.finish(111_000, 110_000, true),
            Err(Error::InvalidBudget)
        );
    }
}

#[test]
fn actual_rate_accounting_precedes_later_publication_evidence_or_failure() {
    let mut pacing = enabled();
    pressure(&mut pacing);
    pacing.start(110_000).unwrap();
    pacing
        .complete_bitrate(111_000, Some(INITIAL / 2), true)
        .unwrap();
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL / 2);
    assert_eq!(pacing.in_flight, Some(110_000));
    assert!(pacing.observation.is_none());
    // A downstream source-observation refusal cannot undo a real encoder effect.
    assert!(pacing.finish(111_000, 111_001, true).is_err());
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL / 2);
}

#[test]
fn cadence_only_callers_keep_legacy_replies_without_a_bitrate_transaction() {
    let mut pacing = pacing();
    pacing.start(10).unwrap();
    assert_eq!(pacing.capture_bitrate(), None);
    pacing.complete_bitrate(20, None, false).unwrap();
    pacing.finish(20, 10, false).unwrap();
    assert!(pacing.bitrate.is_none());
    assert_eq!(pacing.enable_bitrate(INITIAL), Ok(()));
    assert_eq!(pacing.enable_bitrate(INITIAL), Err(Error::InvalidBudget));
}

#[test]
fn dropped_picture_releases_exact_work_credit_without_rate_or_idle_confirmation() {
    let mut pacing = enabled();
    pressure(&mut pacing);
    let decision = policy(&pacing).pending();
    pacing.observation = Some(Observation {
        at_us: 100_000,
        changed: false,
    });
    pacing.start(110_000).unwrap();
    assert_eq!(pacing.finish_skipped(111_000), Err(Error::InvalidBudget));
    pacing.skip_bitrate(111_000, INITIAL / 2).unwrap();
    pacing.finish_skipped(111_000).unwrap();
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL);
    assert_eq!(policy(&pacing).pending(), decision);
    assert_eq!(pacing.in_flight, None);
    assert_eq!(pacing.last_work, Some((111_000, 1000)));
    assert!(pacing.observation.is_none());
    assert_eq!(pacing.finish_skipped(111_001), Err(Error::InvalidBudget));
    pacing.start(120_000).unwrap();
    assert_eq!(pacing.capture_bitrate(), Some(INITIAL / 2));
    completed(&mut pacing, 120_000, INITIAL / 2, true);
    assert_eq!(policy(&pacing).applied_bitrate(), INITIAL / 2);
}

#[test]
fn cadence_only_drop_has_work_cost_and_no_source_observation() {
    let mut pacing = pacing();
    pacing.start(10).unwrap();
    pacing.skip_bitrate(20, INITIAL).unwrap();
    assert!(pacing.finish_skipped(9).is_err());
    assert_eq!(pacing.in_flight, Some(10));
    pacing.finish_skipped(20).unwrap();
    assert_eq!(pacing.last_work, Some((20, 10)));
    assert!(pacing.observation.is_none());
    pacing.start(21).unwrap();
}

fn enabled_profile(profile: WorkstationProfile) -> CapturePacing {
    let mut pacing = CapturePacing::new_with_profile(
        Duration::from_millis(50),
        Duration::from_millis(200),
        30,
        profile,
    )
    .unwrap();
    pacing.enable_bitrate(INITIAL).unwrap();
    pacing
}

#[test]
fn shared_source_applies_the_local_quality_order_to_one_paired_controller() {
    for (profile, expected_interval, expected_target) in [
        (WorkstationProfile::Auto, 200_000, Some(INITIAL / 2)),
        (WorkstationProfile::Text, 200_000, None),
        (WorkstationProfile::Motion, 100_000, Some(INITIAL / 2)),
    ] {
        let mut pacing = enabled_profile(profile);
        let mut views = cohort();
        pacing.observe_receiver_views(&views).unwrap();
        for now in (0..=100_000).step_by(10_000) {
            let blocked = (
                Some((now, Availability::Blocked)),
                ReceiverEvidence::Unobserved,
            );
            let evidence = peer_evidence(now, pacing.interval_us(), [blocked, blocked].into_iter());
            pacing.sample(now, true, evidence).unwrap();
        }
        assert_eq!(pacing.controller.profile(), profile);
        assert_eq!(policy(&pacing).profile(), profile);
        assert_eq!(pacing.interval_us(), expected_interval);
        assert_eq!(
            policy(&pacing).pending().map(bitrate::Change::bitrate),
            expected_target
        );
        assert_eq!(policy(&pacing).applied_bitrate(), INITIAL);
        if let Some(target) = expected_target {
            pacing.start(110_000).unwrap();
            assert_eq!(pacing.capture_bitrate(), Some(target));
            completed(&mut pacing, 110_000, target, true);
            assert_eq!(policy(&pacing).applied_bitrate(), target);
        }
        views[0].as_mut().unwrap().recovery = binding().recovery.next().unwrap();
        pacing.observe_receiver_views(&views).unwrap();
        assert_eq!(pacing.controller.profile(), profile);
        assert_eq!(policy(&pacing).profile(), profile);
    }
}

#[test]
fn quality_preferences_never_let_one_slow_viewer_reduce_a_shared_source() {
    for profile in [
        WorkstationProfile::Auto,
        WorkstationProfile::Text,
        WorkstationProfile::Motion,
    ] {
        for other in [Availability::Ready, Availability::Unknown] {
            let mut pacing = enabled_profile(profile);
            for now in (0..=400_000).step_by(10_000) {
                let blocked = (
                    Some((now, Availability::Blocked)),
                    ReceiverEvidence::Unobserved,
                );
                let sibling = (Some((now, other)), ReceiverEvidence::Unobserved);
                let evidence =
                    peer_evidence(now, pacing.interval_us(), [blocked, sibling].into_iter());
                pacing.sample(now, true, evidence).unwrap();
            }
            assert_eq!(pacing.interval_us(), 100_000);
            assert_eq!(policy(&pacing).applied_bitrate(), INITIAL);
            assert!(policy(&pacing).pending().is_none());
        }
    }
}
