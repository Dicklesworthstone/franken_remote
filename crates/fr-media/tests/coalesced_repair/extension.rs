//! Retain tail requests during paced original transmission; each requested
//! fragment still waits for its own original. Clocks and packet loss are injected.
use super::*;
use fr_media::delivery::BudgetUsage;

#[test]
fn repeated_paced_tail_losses_recover_on_the_last_request_without_replacing_the_chain() {
    let c = config(2);
    let (mut sender, mut receiver) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            ..SendPolicy::default()
        },
    );
    let mut attempts = 0;
    let mut repaired = 0;
    for frame in 1..=24 {
        let start = frame * 300_000;
        let expected = push(&mut sender, c, frame, 8, start);
        let (channel, bytes) = original(&mut sender, start);
        receiver.receive(channel, &bytes, start).unwrap();
        for index in 0..4 {
            let (channel, bytes) = original(&mut sender, start + 15_000);
            if index % 2 == 1 {
                receiver.receive(channel, &bytes, start + 15_000).unwrap();
            }
        }
        let mut packet = [0; 1_150];
        let first = receiver
            .repair_offer(start + 20_000, &mut packet)
            .unwrap()
            .unwrap();
        sender
            .queue_repair(&packet[..first.bytes], start + 20_000)
            .unwrap();
        attempts += 1;
        // The original tail is still paced out. No original or repair arrives
        // before the receiver's LAST permitted request includes that tail.
        let last = receiver
            .repair_offer(start + 80_000, &mut packet)
            .unwrap()
            .unwrap();
        assert_eq!(last.reference_deadline_us, start + 250_000);
        let charged = sender.cached_bytes();
        sender
            .queue_repair(&packet[..last.bytes], start + 80_000)
            .unwrap();
        attempts += 1;
        assert_eq!(sender.cached_bytes(), charged);
        for expected_index in [0, 2, 3] {
            assert_eq!(
                deliver_repair(c, &mut sender, &mut receiver, start + 80_000),
                Some(expected_index)
            );
            repaired += 1;
        }
        for index in 4..8 {
            let now = start + 100_000 + u64::from(index) * 5_000;
            // Neither a repair call nor elapsed time can issue a future original.
            assert!(deliver_repair(c, &mut sender, &mut receiver, now).is_none());
            let _lost_original = original(&mut sender, now);
            assert_eq!(
                deliver_repair(c, &mut sender, &mut receiver, now),
                Some(index)
            );
            repaired += 1;
        }
        assert!(
            sender
                .next_repair_packet(start + 140_000, &mut packet)
                .unwrap()
                .is_none()
        );
        assert!(
            receiver
                .repair_offer(start + 140_000, &mut packet)
                .unwrap()
                .is_none()
        );
        let picture = receiver.take_decodable(start + 140_000).unwrap().unwrap();
        assert_eq!(picture.bytes(), expected);
        assert_eq!(picture.descriptor().frame, frame);
        assert_eq!(picture.epoch(), c.epoch);
        receiver.complete_decode(&picture, start + 140_000).unwrap();
        drop(picture);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert!(!sender.needs_recovery());
    }
    assert_eq!((attempts, repaired), (48, 168));
}

fn deliver_repair(
    c: ReceiveConfig,
    sender: &mut SendCache,
    receiver: &mut ReceivePipeline,
    now: u64,
) -> Option<u32> {
    let mut bytes = [0; 1_150];
    let offer = sender.next_repair_packet(now, &mut bytes).unwrap()?;
    sender.authorize_write(&offer, now).unwrap();
    let record = Record::decode(
        &bytes[..offer.byte_len()],
        &c.limits,
        c.bindings.for_channel(Channel::Video),
        Channel::Video,
    )
    .unwrap();
    let index = decode_fragment(record, &c.limits).unwrap().index;
    receiver
        .receive(offer.channel(), &bytes[..offer.byte_len()], now)
        .unwrap();
    Some(index)
}

#[test]
fn compacted_future_ranges_leave_the_already_issued_prefix_eligible() {
    let c = config(1);
    let (mut sender, _) = setup(c, SendPolicy::default());
    push(&mut sender, c, 1, 8, 1_000);
    for _ in 0..5 {
        original(&mut sender, 1_000);
    }
    request(c, &mut sender, 8, &[(0, 1)], 20_000).unwrap();
    request(c, &mut sender, 8, &[(6, 8)], 40_000).unwrap();
    for index in 0..4 {
        assert_eq!(repair(c, &mut sender, 40_000), Some(index));
    }
    for index in 4..8 {
        assert_eq!(repair(c, &mut sender, 40_001), None);
        original(&mut sender, 40_001);
        assert_eq!(repair(c, &mut sender, 40_001), Some(index));
    }
    assert_eq!(repair(c, &mut sender, 40_001), None);
}

#[test]
fn new_issued_losses_precede_an_extended_future_tail_on_the_same_job() {
    let c = config(2);
    let (mut sender, _) = setup(c, SendPolicy::default());
    push(&mut sender, c, 1, 8, 1_000);
    for _ in 0..5 {
        original(&mut sender, 1_000);
    }
    request(c, &mut sender, 8, &[(6, 7)], 20_000).unwrap();
    assert_eq!(repair(c, &mut sender, 20_000), None);
    request(c, &mut sender, 8, &[(0, 1), (7, 8)], 40_000).unwrap();
    assert_eq!(repair(c, &mut sender, 40_000), Some(0));
    for index in 4..8 {
        assert_eq!(repair(c, &mut sender, 40_001), None);
        original(&mut sender, 40_001);
        assert_eq!(
            repair(c, &mut sender, 40_001),
            (index >= 6).then_some(index)
        );
    }
}

#[test]
fn future_extensions_schedule_byte_refills_only_after_their_own_original_is_issued() {
    let c = config(2);
    let (mut sender, _) = setup(
        c,
        SendPolicy {
            repair_bytes_per_window: 1_150,
            repair_window_micros: 30_000,
            ..SendPolicy::default()
        },
    );
    push(&mut sender, c, 1, 8, 1_000);
    for _ in 0..5 {
        original(&mut sender, 1_000);
    }
    request(c, &mut sender, 8, &[(0, 1)], 20_000).unwrap();
    request(c, &mut sender, 8, &[(6, 8)], 40_000).unwrap();
    assert_eq!(repair(c, &mut sender, 40_000), Some(0));
    assert_eq!(repair(c, &mut sender, 69_999), None);
    assert_eq!(sender.next_deadline(), Some(251_000));
    original(&mut sender, 70_000); // 4
    original(&mut sender, 70_000); // 5
    assert_eq!(repair(c, &mut sender, 70_000), None);
    original(&mut sender, 70_001); // 6
    assert_eq!(repair(c, &mut sender, 70_001), Some(6));
    assert_eq!(sender.next_deadline(), Some(251_000));
    original(&mut sender, 70_002); // 7, now eligible but the byte window is full.
    assert_eq!(repair(c, &mut sender, 70_002), None);
    assert_eq!(sender.next_deadline(), Some(100_000));
    assert_eq!(repair(c, &mut sender, 99_999), None);
    assert_eq!(repair(c, &mut sender, 100_000), Some(7));
    assert_eq!(repair(c, &mut sender, 100_000), None);
}

#[test]
fn accepted_future_extensions_cannot_move_an_unsent_originals_hard_deadline() {
    let c = config(2);
    let (mut sender, _) = setup(c, SendPolicy::default());
    push(&mut sender, c, 1, 8, 1_000);
    for _ in 0..5 {
        original(&mut sender, 1_000);
    }
    request(c, &mut sender, 8, &[(0, 1)], 20_000).unwrap();
    request(c, &mut sender, 8, &[(6, 8)], 240_000).unwrap();
    let mut bytes = [0; 1_150];
    let offer = sender
        .next_repair_packet(250_999, &mut bytes)
        .unwrap()
        .unwrap();
    assert_eq!(offer.send_by_micros(), 251_000);
    assert_eq!(sender.next_deadline(), Some(251_000));
    assert_eq!(
        sender.next_repair_packet(251_000, &mut bytes),
        Err(SendError::OriginalExpired)
    );
    assert!(sender.needs_recovery());
    assert!(sender.authorize_write(&offer, 251_000).is_err());
}

#[test]
fn refused_future_extensions_preserve_the_job_and_do_not_refill_round_credit() {
    let c = config(2);
    let (mut sender, _) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            ..SendPolicy::default()
        },
    );
    push(&mut sender, c, 1, 8, 1_000);
    for _ in 0..5 {
        original(&mut sender, 1_000);
    }
    request(c, &mut sender, 8, &[(0, 1)], 20_000).unwrap();
    assert_eq!(
        request(c, &mut sender, 9, &[(8, 9)], 29_000),
        Err(SendError::Wire(WireError::InvalidRanges))
    );
    assert_eq!(
        request(c, &mut sender, 8, &[(4, 8)], 29_999),
        Err(SendError::RepairBusy)
    );
    request(c, &mut sender, 8, &[(6, 8)], 30_000).unwrap();
    assert_eq!(
        request(c, &mut sender, 8, &[(4, 8)], 50_000),
        Err(SendError::RepairBusy)
    );
    assert_eq!(repair(c, &mut sender, 50_000), Some(0));
    finish(&mut sender, 50_001);
    assert_eq!(repair(c, &mut sender, 50_001), Some(6));
    assert_eq!(repair(c, &mut sender, 50_001), Some(7));
    assert_eq!(repair(c, &mut sender, 50_001), None);
    assert_eq!(
        request(c, &mut sender, 8, &[(4, 5)], 60_000),
        Err(SendError::RepairRateLimited)
    );
}
