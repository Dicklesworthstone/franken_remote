//! Prefix repair composes with the existing one-job early-request retention.
use super::*;

fn request(frame: u64, ranges: &[RepairRange]) -> Vec<u8> {
    let c = config();
    let mut out = vec![0; 1_150];
    let n = encode_repair(
        frame,
        ranges,
        FRAGMENTS,
        c.bindings.for_channel(Channel::Control),
        &c.limits,
        &mut out,
    )
    .unwrap();
    out.truncate(n);
    out
}
fn begin(sender: &mut SendCache, frame: u64, now: u64) -> Vec<u8> {
    let data: Vec<_> = (0_u8..=255)
        .cycle()
        .take(usize::try_from(FRAGMENTS * STRIDE).unwrap())
        .collect();
    sender
        .push(
            progress(frame, data.len(), now),
            data.clone(),
            DeliveryMode::Datagrams,
            now,
        )
        .unwrap();
    data
}
fn issue(sender: &mut SendCache, frame: u64, now: u64, count: u32) {
    begin(sender, frame, now);
    let packet = original(sender, now);
    assert_eq!(packet.0.channel(), Channel::MediaConfig);
    sender.authorize_write(&packet.0, now).unwrap();
    for expected in 0..count {
        let packet = original(sender, now);
        assert_eq!(index(&packet), expected);
        sender.authorize_write(&packet.0, now).unwrap();
    }
}

#[test]
fn an_observed_prefix_hole_is_repaired_before_the_unrelated_original_tail() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let data = begin(&mut sender, 1, 10_000);
    deliver(&mut receiver, &original(&mut sender, 10_000), 30_000);
    for i in 0..3 {
        let packet = original(&mut sender, 10_000);
        sender.authorize_write(&packet.0, 10_000).unwrap();
        if i != 1 {
            deliver(&mut receiver, &packet, 35_000);
        }
    }
    let usage = (sender.cached_pictures(), sender.cached_bytes());
    let mut out = [0; 1_150];
    let query = receiver.repair_offer(50_000, &mut out).unwrap().unwrap();
    sender.queue_repair(&out[..query.bytes], 70_000).unwrap();
    assert!(sender.originals_pending());
    let packet = repair(&mut sender, 70_000).expect("prefix must not wait for unrelated tail");
    assert_eq!(index(&packet), 1);
    assert_eq!(packet.0.send_by_micros(), 260_000);
    sender.authorize_write(&packet.0, 70_000).unwrap();
    deliver(&mut receiver, &packet, 90_000);
    assert!(repair(&mut sender, 70_000).is_none());
    assert_eq!((sender.cached_pictures(), sender.cached_bytes()), usage);
    assert!(receiver.take_decodable(90_000).unwrap().is_none());
    for expected in 3..FRAGMENTS {
        let packet = original(&mut sender, 100_000);
        assert_eq!(
            index(&packet),
            expected,
            "repair cannot move the original cursor"
        );
        sender.authorize_write(&packet.0, 100_000).unwrap();
        deliver(&mut receiver, &packet, 120_000);
    }
    let picture = receiver.take_decodable(120_000).unwrap().unwrap();
    assert_eq!(picture.bytes(), data);
    receiver
        .acknowledge_decode(&picture, true, 120_000)
        .unwrap();
    assert!(!sender.needs_recovery());
}

#[test]
fn sparse_prefix_ranges_finish_before_the_unrequested_tail() {
    let (mut sender, _) = fixture(SendPolicy::default());
    issue(&mut sender, 1, 10_000, 6);
    sender
        .queue_repair(
            &request(
                1,
                &[
                    RepairRange { start: 1, end: 2 },
                    RepairRange { start: 4, end: 6 },
                ],
            ),
            30_000,
        )
        .unwrap();
    for expected in [1, 4, 5] {
        let packet = repair(&mut sender, 30_000).unwrap();
        assert_eq!(index(&packet), expected);
        sender.authorize_write(&packet.0, 30_000).unwrap();
        assert!(sender.originals_pending());
    }
    assert!(repair(&mut sender, 30_000).is_none());
    assert_eq!(index(&original(&mut sender, 30_000)), 6);
}

#[test]
fn a_mixed_request_waits_for_exactly_its_fixed_requested_prefix() {
    let (mut sender, _) = fixture(SendPolicy {
        max_repair_rounds: 1,
        repair_bytes_per_window: 3 * 1_150,
        ..SendPolicy::default()
    });
    issue(&mut sender, 1, 10_000, 1);
    let query = request(1, &[RepairRange { start: 0, end: 3 }]);
    sender.queue_repair(&query, 30_000).unwrap();
    let mut out = [0xa5; 1_150];
    for now in 30_000..30_100 {
        assert!(sender.next_repair_packet(now, &mut out).unwrap().is_none());
        assert_eq!(out, [0xa5; 1_150]);
        assert_eq!(sender.queue_repair(&query, now), Err(SendError::RepairBusy));
    }
    assert_eq!(index(&original(&mut sender, 31_000)), 1);
    assert!(
        sender
            .next_repair_packet(31_000, &mut out)
            .unwrap()
            .is_none()
    );
    assert_eq!(index(&original(&mut sender, 32_000)), 2);
    for expected in 0..3 {
        let packet = repair(&mut sender, 32_000).unwrap();
        assert_eq!(index(&packet), expected);
        assert_eq!(packet.0.byte_len(), 1_150);
        sender.authorize_write(&packet.0, 32_000).unwrap();
    }
    assert!(repair(&mut sender, 32_000).is_none());
    assert_eq!(index(&original(&mut sender, 32_000)), 3);
    assert_eq!(
        sender.queue_repair(&query, 100_000),
        Err(SendError::RepairRateLimited)
    );
}

#[test]
fn prefix_output_refusal_does_not_consume_the_job_or_byte_allowance() {
    let (mut sender, _) = fixture(SendPolicy {
        repair_bytes_per_window: 1_150,
        ..SendPolicy::default()
    });
    issue(&mut sender, 1, 10_000, 2);
    sender
        .queue_repair(&request(1, &[RepairRange { start: 0, end: 1 }]), 30_000)
        .unwrap();
    assert!(matches!(
        sender.next_repair_packet(30_000, &mut []),
        Err(SendError::Wire(_))
    ));
    let packet = repair(&mut sender, 30_000).unwrap();
    assert_eq!(index(&packet), 0);
    assert_eq!(packet.0.byte_len(), 1_150);
    assert!(repair(&mut sender, 30_000).is_none());
}

#[test]
fn prefix_repair_cannot_extend_an_unfinished_originals_hard_deadline() {
    let (mut sender, _) = fixture(SendPolicy::default());
    issue(&mut sender, 1, 10_000, 2);
    sender
        .queue_repair(&request(1, &[RepairRange { start: 0, end: 1 }]), 30_000)
        .unwrap();
    let packet = repair(&mut sender, 30_000).unwrap();
    assert_eq!(packet.0.send_by_micros(), 260_000);
    assert_eq!(
        sender.authorize_write(&packet.0, 260_000),
        Err(SendError::OriginalExpired)
    );
    assert!(sender.needs_recovery());
    assert_eq!(sender.cached_bytes(), 0);
    assert!(sender.next_repair_packet(260_001, &mut [0; 1_150]).is_err());
}

#[test]
fn forty_paced_pictures_with_lost_prefix_repairs_keep_the_same_dependency_chain() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let mut repaired = 0;
    for frame in 1..=40_u64 {
        let now = frame * 300_000;
        let data = begin(&mut sender, frame, now);
        deliver(&mut receiver, &original(&mut sender, now), now);
        for i in 0..3 {
            let packet = original(&mut sender, now);
            sender.authorize_write(&packet.0, now).unwrap();
            if i != 1 {
                deliver(&mut receiver, &packet, now + 10_000);
            }
        }
        let mut out = [0; 1_150];
        let query = receiver
            .repair_offer(now + 20_000, &mut out)
            .unwrap()
            .unwrap();
        sender
            .queue_repair(&out[..query.bytes], now + 30_000)
            .unwrap();
        let packet = repair(&mut sender, now + 30_000).unwrap();
        assert_eq!(index(&packet), 1);
        sender.authorize_write(&packet.0, now + 30_000).unwrap();
        assert!(sender.originals_pending());
        if frame % 3 != 0 {
            deliver(&mut receiver, &packet, now + 40_000);
        }
        repaired += 1;
        assert!(repair(&mut sender, now + 30_000).is_none());
        for expected in 3..FRAGMENTS {
            let packet = original(&mut sender, now + 35_000);
            assert_eq!(index(&packet), expected);
            sender.authorize_write(&packet.0, now + 35_000).unwrap();
            deliver(&mut receiver, &packet, now + 45_000);
        }
        if frame % 3 == 0 {
            assert!(receiver.take_decodable(now + 45_000).unwrap().is_none());
            let query = receiver
                .repair_offer(now + 80_000, &mut out)
                .unwrap()
                .unwrap();
            sender
                .queue_repair(&out[..query.bytes], now + 90_000)
                .unwrap();
            let packet = repair(&mut sender, now + 90_000).unwrap();
            assert_eq!(index(&packet), 1);
            sender.authorize_write(&packet.0, now + 90_000).unwrap();
            deliver(&mut receiver, &packet, now + 100_000);
            repaired += 1;
        }
        let picture = receiver.take_decodable(now + 100_000).unwrap().unwrap();
        assert_eq!(picture.descriptor().frame, frame);
        assert_eq!(picture.epoch(), config().epoch);
        assert_eq!(picture.bytes(), data);
        receiver
            .acknowledge_decode(&picture, true, now + 100_000)
            .unwrap();
        drop(picture);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert!(!sender.needs_recovery());
    }
    assert_eq!(repaired, 53);
}

#[test]
fn later_ranges_cannot_bypass_original_emission_because_the_first_is_ready() {
    let (mut sender, _) = fixture(SendPolicy::default());
    issue(&mut sender, 1, 10_000, 2);
    sender
        .queue_repair(
            &request(
                1,
                &[
                    RepairRange { start: 0, end: 1 },
                    RepairRange { start: 5, end: 6 },
                ],
            ),
            30_000,
        )
        .unwrap();
    let mut out = [0xa5; 1_150];
    for expected in 2..6 {
        assert!(
            sender
                .next_repair_packet(30_000, &mut out)
                .unwrap()
                .is_none()
        );
        assert_eq!(out, [0xa5; 1_150]);
        assert_eq!(index(&original(&mut sender, 30_000)), expected);
    }
    for expected in [0, 5] {
        let packet = repair(&mut sender, 30_000).unwrap();
        assert_eq!(index(&packet), expected);
        sender.authorize_write(&packet.0, 30_000).unwrap();
    }
    assert!(repair(&mut sender, 30_000).is_none());
    assert!(sender.originals_pending());
}
