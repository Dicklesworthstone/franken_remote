//! Exercise repair-range saturation through the actual sender, wire codec and
//! receiver. These byte-pattern tests do not certify HEVC or network performance.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryMode, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline,
    ReceivePolicy, SendCache, SendPolicy,
};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, Record, RepairRange,
    SourceObservation, decode_fragment, decode_repair,
};

fn config(ranges: u16) -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, ranges).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn progress(frame: u64, size: usize, c: ReceiveConfig) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            reference: frame.checked_sub(1),
            total_bytes: u32::try_from(size).unwrap(),
            stride: c.limits.fragment_stride(),
            capture_micros: frame,
        },
        observed_micros: frame,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Idle,
    }
}
fn missing_picture(
    c: ReceiveConfig,
    fragments: u32,
    missing: &[u32],
) -> (SendCache, ReceivePipeline, Vec<u8>) {
    let mut sender = SendCache::new(c.limits, c.bindings, c.epoch, SendPolicy::default()).unwrap();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    sender
        .push(progress(0, 16, c), vec![0; 16], DeliveryMode::Recovery, 0)
        .unwrap();
    let mut packet = [0; 1_150];
    while let Some(offer) = sender.next_packet(0, &mut packet).unwrap() {
        sender.authorize_write(&offer, 0).unwrap();
        receiver
            .receive(offer.channel(), &packet[..offer.byte_len()], 0)
            .unwrap();
    }
    let recovery = receiver.take_decodable(0).unwrap().unwrap();
    receiver.acknowledge_decode(&recovery, true, 0).unwrap();
    drop(recovery);
    let length = usize::try_from(fragments * c.limits.fragment_stride()).unwrap();
    let bytes: Vec<_> = (0_u8..=255).cycle().take(length).collect();
    sender
        .push(
            progress(1, length, c),
            bytes.clone(),
            DeliveryMode::Datagrams,
            1,
        )
        .unwrap();
    while let Some(offer) = sender.next_packet(1, &mut packet).unwrap() {
        sender.authorize_write(&offer, 1).unwrap();
        let wire = &packet[..offer.byte_len()];
        if offer.channel() == Channel::Video {
            let record = Record::decode(
                wire,
                &c.limits,
                c.bindings.for_channel(Channel::Video),
                Channel::Video,
            )
            .unwrap();
            let fragment = decode_fragment(record, &c.limits).unwrap();
            if missing.contains(&fragment.index) {
                continue;
            }
        }
        receiver.receive(offer.channel(), wire, 1).unwrap();
    }
    (sender, receiver, bytes)
}
fn repair_once(c: ReceiveConfig, fragments: u32, missing: &[u32]) -> Vec<RepairRange> {
    let (mut sender, mut receiver, bytes) = missing_picture(c, fragments, missing);
    let mut packet = [0; 1_150];
    let now = 1 + c.policy.repair_delay_micros;
    let offer = receiver.repair_offer(now, &mut packet).unwrap().unwrap();
    let record = Record::decode(
        &packet[..offer.bytes],
        &c.limits,
        c.bindings.for_channel(Channel::Control),
        Channel::Control,
    )
    .unwrap();
    let ranges: Vec<_> = decode_repair(record, fragments, &c.limits)
        .unwrap()
        .ranges()
        .collect();
    assert!(ranges.len() <= usize::from(c.limits.max_repair_ranges()));
    assert_eq!(
        offer.reference_deadline_us,
        1 + c.policy.reference_budget_micros
    );
    sender.queue_repair(&packet[..offer.bytes], now).unwrap();
    while let Some(repair) = sender.next_repair_packet(now, &mut packet).unwrap() {
        sender.authorize_write(&repair, now).unwrap();
        receiver
            .receive(repair.channel(), &packet[..repair.byte_len()], now)
            .unwrap();
    }
    let picture = receiver
        .take_decodable(now)
        .unwrap()
        .expect("one bounded repair round must cover every missing region");
    assert_eq!(picture.descriptor().frame, 1);
    assert_eq!(picture.bytes(), bytes);
    receiver.acknowledge_decode(&picture, true, now).unwrap();
    drop(picture);
    assert_eq!(receiver.budget_usage().pictures, 0);
    assert!(receiver.repair_offer(now, &mut packet).unwrap().is_none());
    ranges
}

#[test]
fn one_range_repairs_all_disjoint_holes_not_just_the_first() {
    assert_eq!(
        repair_once(config(1), 12, &[0, 2, 4, 6, 8, 10]),
        [RepairRange { start: 0, end: 11 }]
    );
}

#[test]
fn saturated_ranges_keep_the_largest_received_gaps_out_of_repair() {
    let ranges = repair_once(config(2), 16, &[0, 2, 7, 9, 10, 15]);
    assert_eq!(
        ranges,
        [
            RepairRange { start: 0, end: 3 },
            RepairRange { start: 7, end: 16 }
        ]
    );
}

#[test]
fn unsaturated_requests_remain_exact_and_do_not_retransmit_received_fragments() {
    assert_eq!(
        repair_once(config(4), 16, &[1, 2, 5, 10, 11, 15]),
        [
            RepairRange { start: 1, end: 3 },
            RepairRange { start: 5, end: 6 },
            RepairRange { start: 10, end: 12 },
            RepairRange { start: 15, end: 16 }
        ]
    );
}

#[test]
fn default_range_ceiling_covers_highly_fragmented_loss_in_one_round() {
    let missing: Vec<_> = (0..256).step_by(2).collect();
    let ranges = repair_once(config(64), 256, &missing);
    assert_eq!(ranges.len(), 64);
}

#[test]
fn all_small_loss_patterns_are_covered_with_minimum_duplicate_fragments() {
    for cap in 1_u16..=4 {
        for mask in 1_u32..256 {
            let missing: Vec<_> = (0..8).filter(|i| mask & (1 << i) != 0).collect();
            let ranges = repair_once(config(cap), 8, &missing);
            let covered: u32 = ranges.iter().map(|r| r.end - r.start).sum();
            // Any legal range cover can exclude at most cap-1 interior received
            // gaps. The largest such gaps give an independent lower byte bound.
            let mut gaps: Vec<_> = missing
                .windows(2)
                .map(|pair| pair[1] - pair[0] - 1)
                .filter(|gap| *gap > 0)
                .collect();
            gaps.sort_unstable_by(|a, b| b.cmp(a));
            let excluded: u32 = gaps.iter().take(usize::from(cap - 1)).sum();
            let optimum = missing.last().unwrap() - missing[0] + 1 - excluded;
            assert_eq!(covered, optimum, "cap={cap} mask={mask}");
        }
    }
}
