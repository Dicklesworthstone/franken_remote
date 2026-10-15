//! Reordering allowance starts when a missing prefix is first exposed, not
//! when the picture was announced. Production sender/receiver and wire codecs;
//! packet times and loss below are injected, not a native transport claim.
use super::*;

#[test]
fn a_new_late_gap_gets_reordering_time_before_spending_a_repair() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    for (packet, at) in [(1, 10_000), (2, 25_000), (3, 40_000), (5, 55_000)] {
        deliver(&mut receiver, &packets[packet], at);
        assert!(requested(&mut receiver, at, FRAGMENTS).is_none());
    }
    // Fragment 4 overtakes 3, well after the picture's original 30 ms probe.
    assert_eq!(receiver.next_deadline(), Some(75_000));
    assert!(requested(&mut receiver, 74_999, FRAGMENTS).is_none());
    deliver(&mut receiver, &packets[4], 74_999);
    assert!(requested(&mut receiver, 75_000, FRAGMENTS).is_none());
    assert_eq!(receiver.next_deadline(), Some(94_999));
    assert_eq!(receiver.reference_deadline(), Some(260_000));
}

#[test]
fn reordered_pairs_preserve_all_retries_for_real_final_fragment_loss() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, 9);
    deliver(&mut receiver, &packets[0], 10_000);
    // Four pairs arrive in reverse order, each gap filled within 10 ms. The
    // overall flight outlives several old picture-anchored repair deadlines.
    for pair in 0..4 {
        let at = 10_000 + u64::try_from(pair).unwrap() * 20_000;
        deliver(&mut receiver, &packets[2 * pair + 2], at);
        assert!(requested(&mut receiver, at, 9).is_none());
        assert!(requested(&mut receiver, at + 9_999, 9).is_none());
        deliver(&mut receiver, &packets[2 * pair + 1], at + 10_000);
        assert!(requested(&mut receiver, at + 10_000, 9).is_none());
    }
    // No repair credit was wasted on reordering. Fragment 8 is really lost.
    for at in [100_000, 160_000, 220_000] {
        assert_eq!(
            requested(&mut receiver, at, 9),
            Some(vec![RepairRange { start: 8, end: 9 }])
        );
    }
    // No reordering observation or retry can move the fixed hard deadline.
    assert_eq!(receiver.next_deadline(), Some(260_000));
    assert_eq!(receiver.tick(260_000), Err(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
}

#[test]
fn continuing_arrivals_and_duplicates_cannot_postpone_an_existing_hole() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    deliver(&mut receiver, &packets[1], 10_000);
    deliver(&mut receiver, &packets[2], 25_000);
    deliver(&mut receiver, &packets[4], 40_000);
    // More unique fragments, duplicate data and repeated progress leave the
    // late hole's 60 ms probe intact instead of restarting its allowance.
    for (packet, at) in [(5, 45_000), (5, 50_000), (0, 55_000), (6, 59_000)] {
        deliver(&mut receiver, &packets[packet], at);
        assert!(requested(&mut receiver, at, FRAGMENTS).is_none());
        assert_eq!(receiver.next_deadline(), Some(60_000));
    }
    assert_eq!(
        requested(&mut receiver, 60_000, FRAGMENTS),
        Some(vec![RepairRange { start: 2, end: 3 }])
    );
    assert_eq!(receiver.reference_deadline(), Some(260_000));
}

#[test]
fn filling_every_hole_resets_grace_only_for_a_new_hole_not_the_retry_cadence() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    deliver(&mut receiver, &packets[2], 10_000);
    assert!(requested(&mut receiver, 29_999, FRAGMENTS).is_none());
    assert!(requested(&mut receiver, 30_000, FRAGMENTS).is_some());
    deliver(&mut receiver, &packets[1], 35_000);
    deliver(&mut receiver, &packets[4], 40_000);
    // The second gap gets its own grace (60 ms), but the previous request's
    // minimum repeat interval (90 ms) is still the stronger gate.
    assert_eq!(receiver.next_deadline(), Some(90_000));
    assert!(requested(&mut receiver, 89_999, FRAGMENTS).is_none());
    assert!(requested(&mut receiver, 90_000, FRAGMENTS).is_some());
}

#[test]
fn a_late_gap_cannot_delay_reference_expiry_even_when_grace_cannot_fit() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    for (packet, at) in [(1, 10_000), (2, 25_000), (3, 40_000), (5, 250_000)] {
        deliver(&mut receiver, &packets[packet], at);
    }
    assert_eq!(receiver.reference_deadline(), Some(260_000));
    assert_eq!(receiver.next_deadline(), Some(260_000));
    assert!(requested(&mut receiver, 259_999, FRAGMENTS).is_none());
    assert_eq!(receiver.tick(260_000), Err(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
}

#[test]
fn a_partial_gap_fill_cannot_restart_grace_for_remaining_holes() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    for (packet, at) in [
        (0, 10_000),
        (1, 10_000),
        (2, 25_000),
        (4, 40_000),
        (6, 50_000),
    ] {
        deliver(&mut receiver, &packets[packet], at);
    }
    deliver(&mut receiver, &packets[3], 55_000);
    assert_eq!(receiver.next_deadline(), Some(60_000));
    assert!(requested(&mut receiver, 59_999, FRAGMENTS).is_none());
    assert_eq!(
        requested(&mut receiver, 60_000, FRAGMENTS),
        Some(vec![RepairRange { start: 4, end: 5 }])
    );
}

#[test]
fn repeated_reordering_and_tail_loss_recover_without_wasting_a_request_or_replacing_the_chain() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let c = config();
    let mut repairs = 0;
    for frame in 1..=24_u64 {
        let start = frame * 300_000;
        let data = vec![u8::try_from(frame).unwrap(); usize::try_from(9 * STRIDE).unwrap()];
        sender
            .push(
                progress(frame, data.len(), start),
                data.clone(),
                DeliveryMode::Datagrams,
                start,
            )
            .unwrap();
        let packets: Vec<_> = (0..=9).map(|_| original(&mut sender, start)).collect();
        for packet in &packets {
            sender.authorize_write(&packet.0, start).unwrap();
        }
        deliver(&mut receiver, &packets[0], start);
        let mut out = [0; 1_150];
        for pair in 0..4 {
            let at = start + u64::try_from(pair).unwrap() * 20_000;
            deliver(&mut receiver, &packets[2 * pair + 2], at);
            assert!(receiver.repair_offer(at, &mut out).unwrap().is_none());
            deliver(&mut receiver, &packets[2 * pair + 1], at + 10_000);
            assert!(
                receiver
                    .repair_offer(at + 10_000, &mut out)
                    .unwrap()
                    .is_none()
            );
        }
        // Only the final ninth fragment was lost; one request must suffice.
        let request = receiver
            .repair_offer(start + 90_000, &mut out)
            .unwrap()
            .unwrap();
        assert_eq!(request.reference_deadline_us, start + 250_000);
        sender
            .queue_repair(&out[..request.bytes], start + 110_000)
            .unwrap();
        let packet = repair(&mut sender, start + 110_000).unwrap();
        assert_eq!(index(&packet), 8);
        sender.authorize_write(&packet.0, start + 110_000).unwrap();
        deliver(&mut receiver, &packet, start + 130_000);
        assert!(repair(&mut sender, start + 110_000).is_none());
        repairs += 1;
        let picture = receiver.take_decodable(start + 130_000).unwrap().unwrap();
        assert_eq!(picture.bytes(), data);
        assert_eq!(picture.epoch(), c.epoch);
        assert_eq!(picture.descriptor().frame, frame);
        receiver.complete_decode(&picture, start + 130_000).unwrap();
        drop(picture);
        assert!(!sender.needs_recovery());
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    }
    assert_eq!(repairs, 24);
}
