//! Actual sender/receiver/wire owners. Explicit clocks, admission pauses and
//! packet loss below are fixtures, not native QUIC or live-path qualification.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryError, DeliveryMode, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig,
    ReceivePipeline, ReceivePolicy, SendCache, SendError, SendPolicy,
};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, Record, RepairRange,
    SourceObservation, WireError, decode_fragment, encode_repair,
};

fn config(maximum: u16) -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, maximum).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy {
            max_repair_attempts: 2,
            ..ReceivePolicy::default()
        },
    }
}
fn push(sender: &mut SendCache, c: ReceiveConfig, frame: u64, count: u32, now: u64) -> Vec<u8> {
    let length = if frame == 0 {
        16
    } else {
        count * c.limits.fragment_stride()
    };
    let bytes: Vec<_> = (0_u8..=255).cycle().take(length as usize).collect();
    sender
        .push(
            Progress {
                descriptor: FrameDescriptor {
                    frame,
                    reference: frame.checked_sub(1),
                    total_bytes: length,
                    stride: c.limits.fragment_stride(),
                    capture_micros: now,
                },
                observed_micros: now,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            bytes.clone(),
            if frame == 0 {
                DeliveryMode::Recovery
            } else {
                DeliveryMode::Datagrams
            },
            now,
        )
        .unwrap();
    bytes
}
fn setup(c: ReceiveConfig, policy: SendPolicy) -> (SendCache, ReceivePipeline) {
    let mut sender = SendCache::new(c.limits, c.bindings, c.epoch, policy).unwrap();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    push(&mut sender, c, 0, 1, 0);
    let mut packet = [0; 1_150];
    while let Some(p) = sender.next_packet(0, &mut packet).unwrap() {
        receiver
            .receive(p.channel(), &packet[..p.byte_len()], 0)
            .unwrap();
    }
    let picture = receiver.take_decodable(0).unwrap().unwrap();
    receiver.complete_decode(&picture, 0).unwrap();
    (sender, receiver)
}
fn original(sender: &mut SendCache, now: u64) -> (Channel, Vec<u8>) {
    let mut packet = [0; 1_150];
    let p = sender.next_packet(now, &mut packet).unwrap().unwrap();
    sender.authorize_write(&p, now).unwrap();
    (p.channel(), packet[..p.byte_len()].to_vec())
}
fn finish(sender: &mut SendCache, now: u64) {
    while sender.next_packet(now, &mut [0; 1_150]).unwrap().is_some() {}
}
fn request(
    c: ReceiveConfig,
    sender: &mut SendCache,
    count: u32,
    ranges: &[(u32, u32)],
    now: u64,
) -> Result<(), SendError> {
    let ranges: Vec<_> = ranges
        .iter()
        .map(|&(start, end)| RepairRange { start, end })
        .collect();
    let mut packet = [0; 1_150];
    let n = encode_repair(
        1,
        &ranges,
        count,
        c.bindings.for_channel(Channel::Control),
        &c.limits,
        &mut packet,
    )
    .unwrap();
    sender.queue_repair(&packet[..n], now)
}
fn repair(c: ReceiveConfig, sender: &mut SendCache, now: u64) -> Option<u32> {
    let mut packet = [0; 1_150];
    let p = sender.next_repair_packet(now, &mut packet).unwrap()?;
    sender.authorize_write(&p, now).unwrap();
    let record = Record::decode(
        &packet[..p.byte_len()],
        &c.limits,
        c.bindings.for_channel(Channel::Video),
        Channel::Video,
    )
    .unwrap();
    Some(decode_fragment(record, &c.limits).unwrap().index)
}

#[test]
fn newer_holes_and_a_lost_repair_survive_one_busy_picture_and_byte_refill() {
    let c = config(64);
    let (mut sender, mut receiver) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            repair_bytes_per_window: 4 * 1_150,
            repair_window_micros: 120_000,
            ..SendPolicy::default()
        },
    );
    let expected = push(&mut sender, c, 1, 8, 1_000);
    let (channel, bytes) = original(&mut sender, 1_000);
    receiver.receive(channel, &bytes, 1_000).unwrap();
    for (index, now) in [(0, 1_000), (1, 12_000), (2, 15_000), (3, 20_000)] {
        let (channel, bytes) = original(&mut sender, now);
        if index % 2 != 0 {
            receiver.receive(channel, &bytes, now).unwrap();
        }
    }
    let mut packet = [0; 1_150];
    let first = receiver.repair_offer(21_000, &mut packet).unwrap().unwrap();
    sender.queue_repair(&packet[..first.bytes], 21_000).unwrap();
    assert_eq!(repair(c, &mut sender, 21_000), Some(0)); // This repair is also lost.
    // The pending request still contains fragment 2 while transport admission
    // is paused. More original data reveals two additional lost fragments.
    for index in 4..8 {
        let (channel, bytes) = original(&mut sender, 40_000);
        if index % 2 != 0 {
            receiver.receive(channel, &bytes, 40_000).unwrap();
        }
    }
    let charged = sender.cached_bytes();
    let last = receiver.repair_offer(81_000, &mut packet).unwrap().unwrap();
    sender.queue_repair(&packet[..last.bytes], 81_000).unwrap();
    assert_eq!(sender.cached_bytes(), charged);
    let mut indices = Vec::new();
    for now in [81_000, 120_000] {
        while let Some(p) = sender.next_repair_packet(now, &mut packet).unwrap() {
            sender.authorize_write(&p, now).unwrap();
            let record = Record::decode(
                &packet[..p.byte_len()],
                &c.limits,
                c.bindings.for_channel(Channel::Video),
                Channel::Video,
            )
            .unwrap();
            indices.push(decode_fragment(record, &c.limits).unwrap().index);
            assert_eq!(p.send_by_micros(), 251_000);
            receiver
                .receive(p.channel(), &packet[..p.byte_len()], now)
                .unwrap();
        }
    }
    assert_eq!(indices, [0, 2, 4, 6]);
    assert!(
        receiver
            .repair_offer(141_000, &mut packet)
            .unwrap()
            .is_none()
    );
    let picture = receiver.take_decodable(141_000).unwrap().unwrap();
    assert_eq!(picture.bytes(), expected);
    receiver.complete_decode(&picture, 141_000).unwrap();
    assert!(!sender.needs_recovery());
    assert_eq!(
        request(c, &mut sender, 8, &[(0, 1)], 141_000),
        Err(SendError::RepairRateLimited)
    );
}

#[test]
fn a_new_request_preserves_only_the_unsent_suffix_unless_repetition_is_explicit() {
    let c = config(64);
    let (mut sender, _) = setup(c, SendPolicy::default());
    push(&mut sender, c, 1, 8, 1_000);
    finish(&mut sender, 1_000);
    request(c, &mut sender, 8, &[(0, 4)], 20_000).unwrap();
    assert_eq!(repair(c, &mut sender, 20_000), Some(0));
    assert_eq!(repair(c, &mut sender, 20_000), Some(1));
    request(c, &mut sender, 8, &[(3, 6)], 40_000).unwrap();
    assert_eq!(
        (0..4)
            .map(|_| repair(c, &mut sender, 40_000).unwrap())
            .collect::<Vec<_>>(),
        [2, 3, 4, 5]
    );
    assert_eq!(repair(c, &mut sender, 40_000), None);
}

#[test]
fn duplicate_and_subset_requests_do_not_consume_rounds_or_restart_the_cursor() {
    let c = config(64);
    let (mut sender, _) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            ..SendPolicy::default()
        },
    );
    push(&mut sender, c, 1, 8, 1_000);
    finish(&mut sender, 1_000);
    request(c, &mut sender, 8, &[(0, 3)], 20_000).unwrap();
    assert_eq!(repair(c, &mut sender, 20_000), Some(0));
    for now in [20_001, 40_000, 60_000] {
        assert_eq!(
            request(c, &mut sender, 8, &[(1, 2)], now),
            Err(SendError::RepairBusy)
        );
    }
    request(c, &mut sender, 8, &[(4, 5)], 60_001).unwrap();
    assert_eq!(
        (0..3)
            .map(|_| repair(c, &mut sender, 60_001).unwrap())
            .collect::<Vec<_>>(),
        [1, 2, 4]
    );
    assert_eq!(
        request(c, &mut sender, 8, &[(6, 7)], 80_000),
        Err(SendError::RepairRateLimited)
    );
}

#[test]
fn overflowing_range_union_retains_new_losses_within_original_retry_credit() {
    let c = config(4);
    let (mut sender, _) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            ..SendPolicy::default()
        },
    );
    push(&mut sender, c, 1, 10, 1_000);
    finish(&mut sender, 1_000);
    request(
        c,
        &mut sender,
        10,
        &[(0, 1), (2, 3), (4, 5), (6, 7)],
        20_000,
    )
    .unwrap();
    let charged = sender.cached_bytes();
    request(c, &mut sender, 10, &[(8, 9)], 40_000).unwrap();
    assert_eq!(sender.cached_bytes(), charged);
    // One real extension spends one round; bridging cannot mint another.
    assert_eq!(
        request(c, &mut sender, 10, &[(1, 6)], 40_001),
        Err(SendError::RepairBusy)
    );
    assert_eq!(
        (0..6)
            .map(|_| repair(c, &mut sender, 40_001).unwrap())
            .collect::<Vec<_>>(),
        [0, 2, 4, 6, 7, 8]
    );
    assert_eq!(repair(c, &mut sender, 40_001), None);
}

#[test]
fn invalid_and_too_frequent_extensions_never_replace_the_pending_job() {
    let c = config(64);
    let (mut sender, _) = setup(
        c,
        SendPolicy {
            max_repair_rounds: 2,
            ..SendPolicy::default()
        },
    );
    push(&mut sender, c, 1, 8, 1_000);
    finish(&mut sender, 1_000);
    request(c, &mut sender, 8, &[(0, 1)], 20_000).unwrap();
    assert_eq!(
        request(c, &mut sender, 9, &[(8, 9)], 20_001),
        Err(SendError::Wire(WireError::InvalidRanges))
    );
    assert_eq!(
        request(c, &mut sender, 8, &[(2, 3)], 20_002),
        Err(SendError::RepairBusy)
    );
    request(c, &mut sender, 8, &[(4, 5)], 30_000).unwrap();
    assert_eq!(repair(c, &mut sender, 30_000), Some(0));
    assert_eq!(repair(c, &mut sender, 30_000), Some(4));
    assert_eq!(repair(c, &mut sender, 30_000), None);
}

#[test]
fn an_unissued_extension_cannot_delay_already_eligible_repair_work() {
    let c = config(64);
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
    } // announcement plus four originals
    request(c, &mut sender, 8, &[(0, 1)], 20_000).unwrap();
    request(c, &mut sender, 8, &[(6, 7)], 40_000).unwrap();
    assert_eq!(repair(c, &mut sender, 40_000), Some(0));
    assert_eq!(repair(c, &mut sender, 40_000), None);
    finish(&mut sender, 40_001);
    // No extra receiver request/round trip after the originals progress.
    assert_eq!(repair(c, &mut sender, 40_002), Some(6));
    assert_eq!(repair(c, &mut sender, 40_002), None);
}

#[test]
fn a_merged_request_keeps_original_expiry_and_generation_fencing() {
    let c = config(64);
    let (mut sender, _) = setup(c, SendPolicy::default());
    push(&mut sender, c, 1, 8, 1_000);
    finish(&mut sender, 1_000);
    request(c, &mut sender, 8, &[(0, 1)], 20_000).unwrap();
    request(c, &mut sender, 8, &[(4, 5)], 200_000).unwrap();
    let mut bytes = [0; 1_150];
    let offer = sender
        .next_repair_packet(250_999, &mut bytes)
        .unwrap()
        .unwrap();
    assert_eq!(offer.send_by_micros(), 251_000);
    assert_eq!(sender.next_repair_packet(251_000, &mut bytes), Ok(None));
    assert_eq!(
        sender.authorize_write(&offer, 251_000),
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
        sender.authorize_write(&offer, 251_001),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
}

#[test]
fn coalescing_cannot_change_a_prepared_transport_packets_bytes_or_deadline() {
    let c = config(64);
    let (mut sender, _) = setup(c, SendPolicy::default());
    push(&mut sender, c, 1, 8, 1_000);
    finish(&mut sender, 1_000);
    request(c, &mut sender, 8, &[(0, 4)], 20_000).unwrap();
    let mut bytes = [0; 1_150];
    let pending = sender
        .next_repair_packet(20_000, &mut bytes)
        .unwrap()
        .unwrap();
    let unchanged = bytes;
    request(c, &mut sender, 8, &[(5, 6)], 40_000).unwrap();
    sender.authorize_write(&pending, 40_000).unwrap();
    assert_eq!(bytes, unchanged);
    assert_eq!(pending.send_by_micros(), 251_000);
    assert_eq!(repair(c, &mut sender, 40_000), Some(1));
    assert_eq!(repair(c, &mut sender, 40_000), Some(2));
    assert_eq!(repair(c, &mut sender, 40_000), Some(3));
    assert_eq!(repair(c, &mut sender, 40_000), Some(5));
    assert_eq!(repair(c, &mut sender, 40_000), None);
}

#[path = "coalesced_repair/compaction.rs"]
mod compaction;

#[path = "coalesced_repair/extension.rs"]
mod extension;
