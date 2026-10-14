//! Real sender, byte codecs and receiver with explicitly injected packet loss.
//! This is not native QUIC, HEVC, or live-tailnet performance qualification.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    DeliveryError, DeliveryMode, MediaBindings, MediaBudget, MediaEpoch, PacketOffer,
    ReceiveConfig, ReceivePipeline, ReceivePolicy, SendCache, SendError, SendPolicy,
};
use fr_wire::{
    Channel, FrameDescriptor, MediaLimits, PipelineState, Progress, RepairRange, SourceObservation,
    WireError, encode_repair,
};

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy {
            max_repair_attempts: 1,
            ..ReceivePolicy::default()
        },
    }
}
fn picture(frame: u64, c: ReceiveConfig) -> (Progress, Vec<u8>) {
    let length = if frame == 0 {
        16
    } else {
        4 * c.limits.fragment_stride()
    };
    let bytes = (0_u8..=255).cycle().take(length as usize).collect();
    (
        Progress {
            descriptor: FrameDescriptor {
                frame,
                reference: frame.checked_sub(1),
                total_bytes: length,
                stride: c.limits.fragment_stride(),
                capture_micros: frame,
            },
            observed_micros: frame,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        bytes,
    )
}
fn next(sender: &mut SendCache, now: u64) -> Option<(PacketOffer, Vec<u8>)> {
    let mut bytes = [0; 1_150];
    let offer = sender.next_packet(now, &mut bytes).unwrap()?;
    sender.authorize_write(&offer, now).unwrap();
    let length = offer.byte_len();
    Some((offer, bytes[..length].to_vec()))
}
fn bootstrap(policy: SendPolicy) -> (SendCache, ReceivePipeline) {
    let c = config();
    let mut sender = SendCache::new(c.limits, c.bindings, c.epoch, policy).unwrap();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    let (progress, bytes) = picture(0, c);
    sender
        .push(progress, bytes, DeliveryMode::Recovery, 0)
        .unwrap();
    while let Some((offer, bytes)) = next(&mut sender, 0) {
        receiver.receive(offer.channel(), &bytes, 0).unwrap();
    }
    let decoded = receiver.take_decodable(0).unwrap().unwrap();
    receiver.acknowledge_decode(&decoded, true, 0).unwrap();
    drop(decoded);
    (sender, receiver)
}
fn announced(sender: &mut SendCache) -> (PacketOffer, Vec<u8>) {
    let (progress, bytes) = picture(1, config());
    sender
        .push(progress, bytes, DeliveryMode::Datagrams, 1)
        .unwrap();
    let announcement = next(sender, 1).unwrap();
    assert_eq!(announcement.0.channel(), Channel::MediaConfig);
    assert!(sender.originals_pending());
    announcement
}
fn request(frame: u64, start: u32, end: u32, count: u32) -> Vec<u8> {
    let c = config();
    let mut bytes = vec![0; 1_150];
    let length = encode_repair(
        frame,
        &[RepairRange { start, end }],
        count,
        c.bindings.for_channel(Channel::Control),
        &c.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(length);
    bytes
}
fn finish_originals(sender: &mut SendCache, now: u64) {
    while next(sender, now).is_some() {}
    assert!(!sender.originals_pending());
}

#[test]
fn an_early_request_repairs_a_lost_prefix_after_original_tail_without_retry() {
    let (mut sender, mut receiver) = bootstrap(SendPolicy::default());
    let (announcement, bytes) = announced(&mut sender);
    receiver.receive(announcement.channel(), &bytes, 1).unwrap();
    // The first data fragment was offered/admitted and then lost. The sender
    // is still congestion-paced on the remainder when the request comes back.
    let (lost, _) = next(&mut sender, 1).unwrap();
    assert_eq!(lost.channel(), Channel::Video);
    let mut packet = [0; 1_150];
    let repair = receiver.repair_offer(20_001, &mut packet).unwrap().unwrap();
    let original_deadline = repair.reference_deadline_us;
    sender
        .queue_repair(&packet[..repair.bytes], 40_001)
        .unwrap();
    // No repair can compete with originals, even across many pending turns.
    for now in [40_001, 80_001, 140_001] {
        assert!(
            sender
                .next_repair_packet(now, &mut packet)
                .unwrap()
                .is_none()
        );
    }
    // Only ONE request was allowed. There can be no successful retry masking
    // a discarded early request, even after its normal retry timer passed.
    assert!(
        receiver
            .repair_offer(140_001, &mut packet)
            .unwrap()
            .is_none()
    );
    while let Some((offer, bytes)) = next(&mut sender, 200_001) {
        receiver.receive(offer.channel(), &bytes, 200_001).unwrap();
    }
    assert!(receiver.take_decodable(200_001).unwrap().is_none());
    let mut repairs = 0;
    while let Some(offer) = sender.next_repair_packet(200_001, &mut packet).unwrap() {
        assert_eq!(offer.send_by_micros(), original_deadline);
        sender.authorize_write(&offer, 200_001).unwrap();
        receiver
            .receive(offer.channel(), &packet[..offer.byte_len()], 220_001)
            .unwrap();
        repairs += 1;
    }
    assert_eq!(repairs, 4);
    let decoded = receiver.take_decodable(220_001).unwrap().unwrap();
    assert_eq!(decoded.descriptor().frame, 1);
    assert_eq!(decoded.bytes(), picture(1, config()).1);
    receiver
        .acknowledge_decode(&decoded, true, 220_001)
        .unwrap();
    drop(decoded);
    assert!(!sender.needs_recovery());
    assert_eq!(receiver.budget_usage().pictures, 0);
}

#[test]
fn waiting_uses_one_job_and_no_repair_bytes_or_extra_rounds() {
    let (mut sender, _) = bootstrap(SendPolicy {
        max_repair_rounds: 1,
        repair_bytes_per_window: 1_150,
        ..SendPolicy::default()
    });
    announced(&mut sender);
    let cached = (sender.cached_pictures(), sender.cached_bytes());
    let repair = request(1, 0, 1, 4);
    sender.queue_repair(&repair, 20_001).unwrap();
    let mut packet = [0xa5; 1_150];
    for now in 20_001..20_101 {
        assert_eq!(
            sender.queue_repair(&repair, now),
            Err(SendError::RepairBusy)
        );
        assert!(
            sender
                .next_repair_packet(now, &mut packet)
                .unwrap()
                .is_none()
        );
        assert_eq!(packet, [0xa5; 1_150]);
        assert_eq!((sender.cached_pictures(), sender.cached_bytes()), cached);
    }
    finish_originals(&mut sender, 100_001);
    let offer = sender
        .next_repair_packet(100_001, &mut packet)
        .unwrap()
        .unwrap();
    assert_eq!(offer.byte_len(), 1_150);
    assert_eq!(offer.send_by_micros(), 250_001);
    assert!(
        sender
            .next_repair_packet(100_001, &mut packet)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sender.queue_repair(&repair, 120_001),
        Err(SendError::RepairRateLimited)
    );
}

#[test]
fn unannounced_reliable_and_out_of_range_requests_still_refuse() {
    let (mut sender, _) = bootstrap(SendPolicy {
        max_repair_rounds: 1,
        ..SendPolicy::default()
    });
    assert_eq!(
        sender.queue_repair(&request(0, 0, 1, 1), 1),
        Err(SendError::RepairNotReady)
    );
    let (progress, bytes) = picture(1, config());
    sender
        .push(progress, bytes, DeliveryMode::Datagrams, 1)
        .unwrap();
    let valid = request(1, 0, 1, 4);
    assert_eq!(
        sender.queue_repair(&valid, 1),
        Err(SendError::RepairNotReady)
    );
    next(&mut sender, 1).unwrap();
    assert_eq!(
        sender.queue_repair(&request(1, 0, 5, 5), 20_001),
        Err(SendError::Wire(WireError::InvalidRanges))
    );
    // None of those refusals consumed the one allowed job or round.
    sender.queue_repair(&valid, 20_001).unwrap();
}

#[test]
fn an_early_request_cannot_extend_an_unfinished_originals_deadline() {
    let (mut sender, _) = bootstrap(SendPolicy::default());
    announced(&mut sender);
    sender.queue_repair(&request(1, 0, 1, 4), 20_001).unwrap();
    assert_eq!(sender.next_deadline(), Some(250_001));
    assert_eq!(
        sender.next_repair_packet(250_001, &mut [0; 1_150]),
        Err(SendError::OriginalExpired)
    );
    assert!(sender.needs_recovery());
    assert_eq!(sender.cached_bytes(), 0);
    assert_eq!(sender.cached_pictures(), 0);
}

#[test]
fn replacement_cancels_the_early_job_and_fences_old_offers() {
    let (mut sender, _) = bootstrap(SendPolicy::default());
    let (old, _) = announced(&mut sender);
    sender.queue_repair(&request(1, 0, 1, 4), 20_001).unwrap();
    let c = config();
    sender
        .replace(
            MediaEpoch {
                recovery: c.epoch.recovery.next().unwrap(),
                ..c.epoch
            },
            MediaBindings::new(5, 6, 7, 8).unwrap(),
            30_001,
        )
        .unwrap();
    assert_eq!(sender.cached_bytes(), 0);
    assert!(
        sender
            .next_repair_packet(30_001, &mut [0; 1_150])
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sender.authorize_write(&old, 30_001),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
    assert!(sender.queue_repair(&request(1, 0, 1, 4), 30_001).is_err());
}

#[test]
fn completed_originals_keep_late_repair_expiry_disposable() {
    let (mut sender, _) = bootstrap(SendPolicy::default());
    announced(&mut sender);
    sender.queue_repair(&request(1, 0, 1, 4), 20_001).unwrap();
    finish_originals(&mut sender, 200_001);
    let offer = sender
        .next_repair_packet(249_999, &mut [0; 1_150])
        .unwrap()
        .unwrap();
    assert_eq!(
        sender.authorize_write(&offer, 250_001),
        Err(SendError::RepairExpired)
    );
    assert!(!sender.needs_recovery());
    assert_eq!(sender.cached_pictures(), 1); // the startup picture has its own 2s horizon
}
