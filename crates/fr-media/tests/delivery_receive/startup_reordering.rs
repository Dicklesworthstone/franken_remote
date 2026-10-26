//! Real record parsers and receiver ownership under cross-channel reordering.
//! Patterned payloads do not claim native HEVC or live-network qualification.
use super::*;

fn starting(c: ReceiveConfig) -> (ReceivePipeline, MediaBudget) {
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    (receiver, budget)
}

fn complete(receiver: &mut ReceivePipeline, c: ReceiveConfig, frame: u64, now: u64) {
    for packet in fragments(descriptor(frame, Some(frame - 1)), &payload(), c) {
        receiver.receive(Channel::Video, &packet, now).unwrap();
    }
}

#[test]
fn datagrams_before_any_recovery_record_wait_for_the_reliable_idr() {
    let c = config();
    let (mut receiver, budget) = starting(c);
    complete(&mut receiver, c, 1, 1);
    assert_eq!(receiver.state(), ReceiveState::AwaitingRecovery);
    assert_eq!(budget.usage().pictures, 1);
    assert!(receiver.take_decodable(2).unwrap().is_none());
    receiver
        .receive(
            Channel::MediaConfig,
            &progress(descriptor(1, Some(0)), c),
            3,
        )
        .unwrap();
    recovery(&mut receiver, c, 100_000);
    assert_eq!(budget.usage().pictures, 2);
    let idr = receiver.take_decodable(100_001).unwrap().unwrap();
    assert_eq!(idr.descriptor().frame, 0);
    assert_eq!(idr.reference_deadline_us(), c.policy.recovery_budget_micros);
    assert!(receiver.take_decodable(100_002).unwrap().is_none());
    receiver.acknowledge_decode(&idr, true, 100_003).unwrap();
    drop(idr);
    let dependent = receiver.take_decodable(100_004).unwrap().unwrap();
    assert_eq!(dependent.descriptor().frame, 1);
    assert_eq!(dependent.bytes(), payload());
    assert_eq!(dependent.reference_deadline_us(), 250_001);
    assert_eq!(dependent.display_deadline_us(), 50_001);
    assert!(!dependent.within_display_queue_budget());
    receiver
        .acknowledge_decode(&dependent, true, 100_005)
        .unwrap();
    drop(dependent);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn later_independent_progress_cannot_replace_the_pending_reliable_idr() {
    let c = config();
    let (mut receiver, budget) = starting(c);
    // A new shared-source IDR's datagrams can overtake the bootstrap stream.
    for packet in fragments(descriptor(5, None), &payload(), c) {
        receiver.receive(Channel::Video, &packet, 1).unwrap();
    }
    assert!(receiver.take_decodable(1).unwrap().is_none());
    // Announcements remain ordered on their reliable stream. The first one
    // identifies the bootstrap; a later independent frame is ordinary media.
    receiver
        .receive(Channel::MediaConfig, &progress(descriptor(0, None), c), 2)
        .unwrap();
    receiver
        .receive(Channel::MediaConfig, &progress(descriptor(5, None), c), 3)
        .unwrap();
    complete(&mut receiver, c, 6, 4);
    recovery(&mut receiver, c, 5);
    let idr = receiver.take_decodable(6).unwrap().unwrap();
    assert_eq!(idr.descriptor().frame, 0);
    receiver.acknowledge_decode(&idr, true, 7).unwrap();
    drop(idr);
    for (frame, now) in [(5, 8), (6, 10)] {
        let picture = receiver.take_decodable(now).unwrap().unwrap();
        assert_eq!(picture.descriptor().frame, frame);
        receiver
            .acknowledge_decode(&picture, true, now + 1)
            .unwrap();
    }
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn unseeded_datagrams_reserve_one_picture_for_the_reliable_idr() {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(2),
        ..LimitOverrides::default()
    })
    .unwrap();
    let c = ReceiveConfig {
        limits: MediaLimits::new(protocol, 1_150, 16_384, 64).unwrap(),
        ..config()
    };
    let (mut receiver, budget) = starting(c);
    complete(&mut receiver, c, 1, 1);
    let retained = budget.usage();
    for packet in fragments(descriptor(2, Some(1)), &payload(), c) {
        assert_eq!(
            receiver.receive(Channel::Video, &packet, 2),
            Ok(ReceiveUpdate::Deferred)
        );
        assert_eq!(budget.usage(), retained);
    }
    recovery(&mut receiver, c, 3);
    for (frame, now) in [(0, 4), (1, 6)] {
        let picture = receiver.take_decodable(now).unwrap().unwrap();
        assert_eq!(picture.descriptor().frame, frame);
        receiver
            .acknowledge_decode(&picture, true, now + 1)
            .unwrap();
    }
    // The ordinary announcement still detects a wholly dropped early picture.
    receiver
        .receive(
            Channel::MediaConfig,
            &progress(descriptor(2, Some(1)), c),
            8,
        )
        .unwrap();
    let mut bytes = [0; 1_150];
    let repair = receiver.repair_offer(20_008, &mut bytes).unwrap().unwrap();
    assert_eq!(repair.frame, 2);
    complete(&mut receiver, c, 2, 20_009);
    let picture = receiver.take_decodable(20_010).unwrap().unwrap();
    assert_eq!(picture.descriptor().frame, 2);
    receiver.acknowledge_decode(&picture, true, 20_011).unwrap();
    drop(picture);
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn insufficient_unseeded_byte_headroom_defers_without_poisoning_startup() {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        max_encoded_access_unit_bytes: Some(4_096),
        per_viewer_compressed_bytes: Some(4_096),
        ..LimitOverrides::default()
    })
    .unwrap();
    let c = ReceiveConfig {
        limits: MediaLimits::new(protocol, 1_150, 16_384, 64).unwrap(),
        ..config()
    };
    let (mut receiver, budget) = starting(c);
    for packet in fragments(descriptor(1, Some(0)), &payload(), c) {
        assert_eq!(
            receiver.receive(Channel::Video, &packet, 1),
            Ok(ReceiveUpdate::Deferred)
        );
    }
    assert_eq!(budget.usage(), BudgetUsage::default());
    assert_eq!(receiver.state(), ReceiveState::AwaitingRecovery);
    recovery(&mut receiver, c, 2);
    let idr = receiver.take_decodable(3).unwrap().unwrap();
    receiver.acknowledge_decode(&idr, true, 4).unwrap();
    drop(idr);
    complete(&mut receiver, c, 1, 5);
    let dependent = receiver.take_decodable(6).unwrap().unwrap();
    assert_eq!(dependent.descriptor().frame, 1);
}

#[test]
fn reserved_bytes_admit_the_maximum_recovery_picture_alongside_early_media() {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(2),
        max_encoded_access_unit_bytes: Some(4_096),
        per_viewer_compressed_bytes: Some(8_192),
        ..LimitOverrides::default()
    })
    .unwrap();
    let c = ReceiveConfig {
        limits: MediaLimits::new(protocol, 1_150, 16_384, 64).unwrap(),
        ..config()
    };
    let (mut receiver, budget) = starting(c);
    complete(&mut receiver, c, 1, 1);
    assert_eq!(budget.usage().pictures, 1);
    let bytes = vec![9; 4_096];
    let mut packet = vec![0; c.limits.record_bytes()];
    for (index, chunk) in bytes.chunks(1_077).enumerate() {
        let n = encode_recovery(
            RecoveryChunk {
                frame: 0,
                total_bytes: 4_096,
                offset: u32::try_from(index * 1_077).unwrap(),
                capture_micros: 123,
                bytes: chunk,
            },
            c.bindings.for_channel(Channel::Recovery),
            &c.limits,
            &mut packet,
        )
        .unwrap();
        receiver
            .receive(Channel::Recovery, &packet[..n], 2)
            .unwrap();
    }
    assert_eq!(budget.usage().pictures, 2);
    assert!(budget.usage().bytes <= 8_192);
    let idr = receiver.take_decodable(3).unwrap().unwrap();
    assert_eq!(idr.bytes(), bytes);
    receiver.acknowledge_decode(&idr, true, 4).unwrap();
    drop(idr);
    let dependent = receiver.take_decodable(5).unwrap().unwrap();
    assert_eq!(dependent.descriptor().frame, 1);
    assert_eq!(dependent.reference_deadline_us(), 250_001);
}

#[test]
fn unretainable_reliable_announcements_remain_explicit_resource_failures() {
    let protocol = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(2),
        ..LimitOverrides::default()
    })
    .unwrap();
    let c = ReceiveConfig {
        limits: MediaLimits::new(protocol, 1_150, 16_384, 64).unwrap(),
        ..config()
    };
    let (mut receiver, budget) = starting(c);
    complete(&mut receiver, c, 1, 1);
    assert_eq!(
        receiver.receive(
            Channel::MediaConfig,
            &progress(descriptor(2, Some(1)), c),
            2
        ),
        Err(DeliveryError::ResourceLimit)
    );
    assert_eq!(receiver.failure(), Some(DeliveryError::ResourceLimit));
    assert_eq!(budget.usage(), BudgetUsage::default());
}

#[test]
fn unseeded_duplicates_do_not_renew_deadlines_and_conflicts_still_fence() {
    let c = config();
    let packet = &fragments(descriptor(1, Some(0)), &payload(), c)[0];
    let (mut receiver, budget) = starting(c);
    receiver.receive(Channel::Video, packet, 1).unwrap();
    let retained = budget.usage();
    for now in [2, 100_000, 250_000] {
        assert_eq!(
            receiver.receive(Channel::Video, packet, now),
            Ok(ReceiveUpdate::Duplicate)
        );
        assert_eq!(budget.usage(), retained);
        assert_eq!(receiver.reference_deadline(), Some(250_001));
    }
    assert_eq!(receiver.tick(250_001), Err(DeliveryError::ReferenceExpired));
    assert_eq!(budget.usage(), BudgetUsage::default());

    let (mut receiver, budget) = starting(c);
    receiver.receive(Channel::Video, packet, 1).unwrap();
    let mut conflicting = packet.clone();
    *conflicting.last_mut().unwrap() ^= 1;
    assert_eq!(
        receiver.receive(Channel::Video, &conflicting, 2),
        Err(DeliveryError::ConflictingPicture)
    );
    assert_eq!(budget.usage(), BudgetUsage::default());
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
}

#[test]
fn unseeded_buffers_do_not_survive_replacement_or_close() {
    let c = config();
    let packet = &fragments(descriptor(1, Some(0)), &payload(), c)[0];
    let (mut receiver, budget) = starting(c);
    receiver.receive(Channel::Video, packet, 1).unwrap();
    receiver
        .replace(
            MediaEpoch {
                recovery: RecoveryGeneration::from_raw(2),
                ..c.epoch
            },
            MediaBindings::new(5, 6, 7, 8).unwrap(),
            2,
        )
        .unwrap();
    assert_eq!(budget.usage(), BudgetUsage::default());
    assert_eq!(
        receiver.receive(Channel::Video, packet, 3),
        Err(DeliveryError::StaleGeneration)
    );
    assert_eq!(receiver.state(), ReceiveState::AwaitingConfiguration);
    let (mut receiver, budget) = starting(c);
    receiver.receive(Channel::Video, packet, 1).unwrap();
    receiver.close();
    assert_eq!(budget.usage(), BudgetUsage::default());
    assert_eq!(
        receiver.receive(Channel::Video, packet, 2),
        Err(DeliveryError::WrongState)
    );
}
