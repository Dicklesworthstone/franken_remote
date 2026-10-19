//! Real sender/receiver and wire codecs with injected loss, reordering and
//! admission pauses. No native network or codec qualification is implied.
use super::*;
use fr_media::delivery::BudgetUsage;
use fr_wire::decode_repair;

#[test]
#[allow(clippy::too_many_lines)] // One ordered loss/reordering timeline, with both attempts visible.
fn the_last_receiver_attempt_recovers_new_holes_when_pending_union_overflows() {
    let c = config(2);
    let (mut sender, mut receiver) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            repair_bytes_per_window: 3 * 1_150,
            repair_window_micros: 120_000,
            ..SendPolicy::default()
        },
    );
    let expected = push(&mut sender, c, 1, 10, 1_000);
    let (channel, bytes) = original(&mut sender, 1_000);
    receiver.receive(channel, &bytes, 1_000).unwrap();
    let mut delayed = None;
    for index in 0..6 {
        let (channel, bytes) = original(&mut sender, 20_000);
        match index {
            0 => delayed = Some((channel, bytes)),
            4 => {} // Actual loss, not reordering.
            _ => {
                receiver.receive(channel, &bytes, 20_000).unwrap();
            }
        }
    }
    let mut packet = [0; 1_150];
    let first = receiver.repair_offer(21_000, &mut packet).unwrap().unwrap();
    let decoded = decode_repair(
        Record::decode(
            &packet[..first.bytes],
            &c.limits,
            c.bindings.for_channel(Channel::Control),
            Channel::Control,
        )
        .unwrap(),
        10,
        &c.limits,
    )
    .unwrap();
    assert_eq!(
        decoded.ranges().collect::<Vec<_>>(),
        [
            RepairRange { start: 0, end: 1 },
            RepairRange { start: 4, end: 5 },
        ]
    );
    sender.queue_repair(&packet[..first.bytes], 21_000).unwrap();
    // Admission stalls that job. The delayed original fills its first hole,
    // while the rest of the original flight exposes a new lost fragment.
    let (channel, bytes) = delayed.unwrap();
    receiver.receive(channel, &bytes, 30_000).unwrap();
    for index in 6..10 {
        let (channel, bytes) = original(&mut sender, 40_000);
        if index != 8 {
            receiver.receive(channel, &bytes, 40_000).unwrap();
        }
    }
    let charged = sender.cached_bytes();
    let last = receiver.repair_offer(81_000, &mut packet).unwrap().unwrap();
    assert_eq!(last.reference_deadline_us, 251_000);
    let decoded = decode_repair(
        Record::decode(
            &packet[..last.bytes],
            &c.limits,
            c.bindings.for_channel(Channel::Control),
            Channel::Control,
        )
        .unwrap(),
        10,
        &c.limits,
    )
    .unwrap();
    assert_eq!(
        decoded.ranges().collect::<Vec<_>>(),
        [
            RepairRange { start: 4, end: 5 },
            RepairRange { start: 8, end: 9 },
        ]
    );
    // Old exact union required three slots and rejected this LAST request.
    sender.queue_repair(&packet[..last.bytes], 81_000).unwrap();
    assert_eq!(sender.cached_bytes(), charged);
    let mut indices = Vec::new();
    for now in [81_000, 120_000] {
        let mut emitted = 0;
        while let Some(offer) = sender.next_repair_packet(now, &mut packet).unwrap() {
            emitted += 1;
            sender.authorize_write(&offer, now).unwrap();
            assert_eq!(offer.send_by_micros(), 251_000);
            let record = Record::decode(
                &packet[..offer.byte_len()],
                &c.limits,
                c.bindings.for_channel(Channel::Video),
                Channel::Video,
            )
            .unwrap();
            indices.push(decode_fragment(record, &c.limits).unwrap().index);
            receiver
                .receive(offer.channel(), &packet[..offer.byte_len()], now)
                .unwrap();
        }
        assert_eq!(emitted, 3); // Bridged duplicates consume real byte allowance.
        if now == 81_000 {
            assert!(receiver.take_decodable(now).unwrap().is_none());
            assert_eq!(sender.next_deadline(), Some(120_000));
        }
    }
    assert_eq!(indices, [0, 4, 5, 6, 7, 8]);
    assert!(
        receiver
            .repair_offer(141_000, &mut packet)
            .unwrap()
            .is_none()
    );
    let picture = receiver.take_decodable(141_000).unwrap().unwrap();
    assert_eq!(picture.bytes(), expected);
    assert_eq!(picture.epoch(), c.epoch);
    receiver.complete_decode(&picture, 141_000).unwrap();
    drop(picture);
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    assert!(!sender.needs_recovery());
    assert_eq!(
        request(c, &mut sender, 10, &[(8, 9)], 141_000),
        Err(SendError::RepairRateLimited)
    );
}

#[test]
fn compacted_jobs_keep_cadence_output_admission_and_duplicate_round_accounting() {
    let c = config(2);
    let (mut sender, _) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 3,
            repair_bytes_per_window: 1_150,
            repair_window_micros: 30_000,
            ..SendPolicy::default()
        },
    );
    push(&mut sender, c, 1, 10, 1_000);
    finish(&mut sender, 1_000);
    request(c, &mut sender, 10, &[(0, 1), (4, 5)], 20_000).unwrap();
    assert_eq!(
        request(c, &mut sender, 10, &[(8, 9)], 29_999),
        Err(SendError::RepairBusy)
    );
    request(c, &mut sender, 10, &[(8, 9)], 30_000).unwrap();
    assert!(sender.next_repair_packet(30_000, &mut []).is_err());
    assert_eq!(repair(c, &mut sender, 30_000), Some(0));
    assert_eq!(repair(c, &mut sender, 30_000), None);
    // A requested bridge duplicate was already included by compaction. It
    // neither restarts this job nor consumes the remaining request round.
    assert_eq!(
        request(c, &mut sender, 10, &[(5, 6)], 40_000),
        Err(SendError::RepairBusy)
    );
    request(c, &mut sender, 10, &[(9, 10)], 40_001).unwrap();
    assert_eq!(sender.next_deadline(), Some(60_000));
    let mut indices = Vec::new();
    for now in [60_000, 90_000, 120_000, 150_000, 180_000, 210_000] {
        indices.push(repair(c, &mut sender, now).unwrap());
        assert_eq!(repair(c, &mut sender, now), None);
    }
    assert_eq!(indices, [4, 5, 6, 7, 8, 9]);
    assert_eq!(
        request(c, &mut sender, 10, &[(0, 1)], 220_000),
        Err(SendError::RepairRateLimited)
    );
}

#[test]
fn full_negotiated_metadata_capacity_covers_both_disjoint_requests_without_growing() {
    let c = config(64);
    let (mut sender, _) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            ..SendPolicy::default()
        },
    );
    push(&mut sender, c, 1, 256, 1_000);
    finish(&mut sender, 1_000);
    let a: Vec<_> = (0..64).map(|i| (4 * i, 4 * i + 1)).collect();
    let b: Vec<_> = (0..64).map(|i| (4 * i + 2, 4 * i + 3)).collect();
    let charged = sender.cached_bytes();
    request(c, &mut sender, 256, &a, 20_000).unwrap();
    request(c, &mut sender, 256, &b, 40_000).unwrap();
    assert_eq!(sender.cached_bytes(), charged);
    let mut indices = Vec::new();
    while let Some(index) = repair(c, &mut sender, 40_000) {
        indices.push(index);
    }
    assert_eq!(indices.len(), 128 + 64); // Exactly 64 minimum-cost bridges.
    for index in (0..256).step_by(2) {
        assert!(indices.contains(&index));
    }
    assert!(indices.windows(2).all(|pair| pair[0] < pair[1]));
    assert!(indices.iter().all(|index| *index < 255));
    assert_eq!(sender.cached_bytes(), charged);
}

#[test]
fn compacted_duplicates_cannot_extend_the_original_horizon_or_survive_replacement() {
    let c = config(1);
    let (mut sender, _) = setup(c, SendPolicy::default());
    push(&mut sender, c, 1, 10, 1_000);
    finish(&mut sender, 1_000);
    request(c, &mut sender, 10, &[(0, 1)], 20_000).unwrap();
    request(c, &mut sender, 10, &[(8, 9)], 240_000).unwrap();
    let mut bytes = [0; 1_150];
    let prepared = sender
        .next_repair_packet(250_999, &mut bytes)
        .unwrap()
        .unwrap();
    assert_eq!(prepared.send_by_micros(), 251_000);
    assert_eq!(sender.next_repair_packet(251_000, &mut bytes), Ok(None));
    assert_eq!(
        sender.authorize_write(&prepared, 251_000),
        Err(SendError::RepairExpired)
    );
    sender
        .replace(
            MediaEpoch {
                recovery: c.epoch.recovery.next().unwrap(),
                ..c.epoch
            },
            MediaBindings::new(5, 6, 7, 8).unwrap(),
            251_001,
        )
        .unwrap();
    assert_eq!(sender.next_repair_packet(251_001, &mut bytes), Ok(None));
    assert_eq!(
        sender.authorize_write(&prepared, 251_001),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
}
