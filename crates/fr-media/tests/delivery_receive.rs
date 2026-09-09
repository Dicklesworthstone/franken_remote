//! These tests transport actual byte records through production reassembly.
//! Payload patterns are deliberately not claimed to be HEVC or live-network evidence.
use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_core::limits::{LimitOverrides, ProtocolLimits};
use fr_media::delivery::*;
use fr_wire::*;

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1_150, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn payload() -> Vec<u8> {
    (0_u8..=255).cycle().take(2_500).collect()
}
fn descriptor(frame: u64, reference: Option<u64>) -> FrameDescriptor {
    FrameDescriptor {
        frame,
        reference,
        total_bytes: 2_500,
        stride: 1_077,
        capture_micros: 123,
    }
}
fn fragments(d: FrameDescriptor, bytes: &[u8], c: ReceiveConfig) -> Vec<Vec<u8>> {
    (0..d.fragment_count().unwrap())
        .map(|index| {
            let mut packet = vec![0; c.limits.record_bytes()];
            let n = encode_fragment(
                Fragment {
                    descriptor: d,
                    index,
                    bytes: &bytes[d.fragment_range(index).unwrap()],
                },
                c.bindings.for_channel(Channel::Video),
                &c.limits,
                &mut packet,
            )
            .unwrap();
            packet.truncate(n);
            packet
        })
        .collect()
}
fn recovery(receiver: &mut ReceivePipeline, c: ReceiveConfig, now: u64) {
    let bytes = payload();
    for (index, chunk) in bytes.chunks(1_077).enumerate() {
        let mut packet = [0; 1_150];
        let n = encode_recovery(
            RecoveryChunk {
                frame: 0,
                total_bytes: 2_500,
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
            .receive(Channel::Recovery, &packet[..n], now)
            .unwrap();
    }
}
fn running(c: ReceiveConfig) -> (ReceivePipeline, MediaBudget) {
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    recovery(&mut receiver, c, 0);
    let picture = receiver.take_decodable(0).unwrap().unwrap();
    assert_eq!(picture.bytes(), payload());
    receiver.acknowledge_decode(&picture, true, 0).unwrap();
    drop(picture);
    (receiver, budget)
}
fn progress(d: FrameDescriptor, c: ReceiveConfig) -> Vec<u8> {
    let mut packet = vec![0; c.limits.record_bytes()];
    let n = encode_progress(
        Progress {
            descriptor: d,
            observed_micros: 123,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Idle,
        },
        c.bindings.for_channel(Channel::MediaConfig),
        &c.limits,
        &mut packet,
    )
    .unwrap();
    packet.truncate(n);
    packet
}

#[test]
fn configured_recovery_and_decode_are_distinct_non_circular_milestones() {
    let c = config();
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    assert_eq!(receiver.state(), ReceiveState::AwaitingConfiguration);
    let packets = fragments(descriptor(1, Some(0)), &payload(), c);
    assert_eq!(
        receiver.receive(Channel::Video, &packets[0], 0),
        Err(DeliveryError::WrongState)
    );
    assert_eq!(budget.usage().pictures, 0);
    receiver.decoder_configured(0).unwrap();
    assert!(receiver.take_decodable(0).unwrap().is_none());
    recovery(&mut receiver, c, 1);
    for packet in &packets {
        receiver.receive(Channel::Video, packet, 2).unwrap();
    }
    let idr = receiver.take_decodable(3).unwrap().unwrap();
    assert_eq!(idr.descriptor().frame, 0);
    assert_eq!(receiver.state(), ReceiveState::DecodingRecovery);
    assert!(receiver.take_decodable(3).unwrap().is_none());
    receiver.acknowledge_decode(&idr, true, 4).unwrap();
    let predicted = receiver.take_decodable(4).unwrap().unwrap();
    assert_eq!(predicted.descriptor().frame, 1);
    assert_eq!(predicted.bytes(), payload());
}

#[test]
fn decoder_owner_rejects_equal_numeric_receivers_and_retired_epochs() {
    let c = config();
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    let mut foreign = ReceivePipeline::new(c, budget.clone()).unwrap();
    let binding = receiver
        .bind_decoder(c.epoch.configuration, c.limits.protocol(), 0)
        .unwrap();
    assert_eq!(binding.check(&receiver), Ok(()));
    assert_eq!(binding.check(&foreign), Err(DeliveryError::DecodeMismatch));
    assert_eq!(
        binding.check_recovery(&foreign),
        Err(DeliveryError::DecodeMismatch)
    );
    assert_eq!(foreign.state(), ReceiveState::AwaitingConfiguration);
    assert_eq!(budget.usage(), BudgetUsage::default());
    // Loss invalidates the existing epoch but leaves a same-owner recovery path.
    assert_eq!(
        receiver.tick(c.policy.recovery_budget_micros),
        Err(DeliveryError::RecoveryExpired)
    );
    assert_eq!(binding.check(&receiver), Err(DeliveryError::DecodeMismatch));
    assert_eq!(binding.check_recovery(&receiver), Ok(()));
    receiver
        .replace(
            MediaEpoch {
                recovery: RecoveryGeneration::from_raw(2),
                ..c.epoch
            },
            MediaBindings::new(5, 6, 7, 8).unwrap(),
            c.policy.recovery_budget_micros,
        )
        .unwrap();
    assert_eq!(
        binding.check_recovery(&receiver),
        Err(DeliveryError::DecodeMismatch)
    );
    let mut replacement = receiver
        .bind_decoder(
            c.epoch.configuration,
            c.limits.protocol(),
            c.policy.recovery_budget_micros,
        )
        .unwrap();
    replacement.revoke();
    assert_eq!(
        replacement.check_recovery(&receiver),
        Err(DeliveryError::WrongState)
    );
    assert_eq!(
        receiver.tick(c.policy.recovery_budget_micros),
        Err(DeliveryError::WrongState)
    );
    assert_eq!(receiver.state(), ReceiveState::Closed);
    assert_eq!(
        replacement.check_recovery(&receiver),
        Err(DeliveryError::WrongState)
    );
    foreign.close();
}

#[test]
fn decoder_binding_requires_matching_admitted_configuration_and_limits() {
    let c = config();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    assert_eq!(
        receiver.check_decoder_configuration(
            CodecConfigurationGeneration::from_raw(2),
            c.limits.protocol()
        ),
        Err(DeliveryError::StaleGeneration)
    );
    let smaller = ProtocolLimits::with_overrides(LimitOverrides {
        max_encoded_access_unit_bytes: Some(1024),
        ..LimitOverrides::default()
    })
    .unwrap();
    assert_eq!(
        receiver.check_decoder_configuration(c.epoch.configuration, &smaller),
        Err(DeliveryError::ResourceLimit)
    );
    assert_eq!(receiver.state(), ReceiveState::AwaitingConfiguration);
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    receiver
        .bind_decoder(c.epoch.configuration, c.limits.protocol(), 0)
        .unwrap();
    assert_eq!(
        receiver.check_decoder_configuration(c.epoch.configuration, c.limits.protocol()),
        Err(DeliveryError::WrongState)
    );
}
#[test]
fn out_of_order_duplicates_reassemble_once_without_extra_budget() {
    let c = config();
    let (mut receiver, budget) = running(c);
    let bytes = payload();
    let packets = fragments(descriptor(1, Some(0)), &bytes, c);
    receiver.receive(Channel::Video, &packets[2], 1).unwrap();
    let reserved = budget.usage();
    assert_eq!(
        receiver.receive(Channel::Video, &packets[2], 2),
        Ok(ReceiveUpdate::Duplicate)
    );
    assert_eq!(budget.usage(), reserved);
    receiver.receive(Channel::Video, &packets[0], 3).unwrap();
    assert!(receiver.take_decodable(3).unwrap().is_none());
    assert_eq!(
        receiver.receive(Channel::Video, &packets[1], 4),
        Ok(ReceiveUpdate::PictureComplete)
    );
    let picture = receiver.take_decodable(4).unwrap().unwrap();
    assert_eq!(picture.bytes(), bytes);
    receiver.acknowledge_decode(&picture, true, 5).unwrap();
    assert_eq!(
        receiver.receive(Channel::Video, &packets[0], 5),
        Ok(ReceiveUpdate::Obsolete)
    );
    assert_eq!(budget.usage(), reserved);
    drop(picture);
    assert_eq!(budget.usage(), BudgetUsage::default());
}
#[test]
fn conflicting_duplicate_fences_the_chain_and_frees_pending_buffers() {
    let c = config();
    let (mut receiver, budget) = running(c);
    let mut packets = fragments(descriptor(1, Some(0)), &payload(), c);
    receiver.receive(Channel::Video, &packets[0], 1).unwrap();
    *packets[0].last_mut().unwrap() ^= 0xff;
    assert_eq!(
        receiver.receive(Channel::Video, &packets[0], 2),
        Err(DeliveryError::ConflictingPicture)
    );
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert_eq!(budget.usage(), BudgetUsage::default());
    assert!(receiver.take_decodable(3).is_err());
}
#[test]
fn changed_descriptor_is_not_a_second_picture_with_the_same_id() {
    let c = config();
    let (mut receiver, _) = running(c);
    let mut d = descriptor(1, Some(0));
    let a = fragments(d, &payload(), c);
    receiver.receive(Channel::Video, &a[0], 1).unwrap();
    d.capture_micros += 1;
    let b = fragments(d, &payload(), c);
    assert_eq!(
        receiver.receive(Channel::Video, &b[1], 2),
        Err(DeliveryError::ConflictingPicture)
    );
}
#[test]
fn stale_for_display_reference_still_unlocks_fresh_dependent_picture() {
    let c = config();
    let (mut receiver, _) = running(c);
    let first = fragments(descriptor(1, Some(0)), &payload(), c);
    let second = fragments(descriptor(2, Some(1)), &payload(), c);
    receiver.receive(Channel::Video, &first[0], 10).unwrap();
    for packet in &second {
        receiver.receive(Channel::Video, packet, 50_000).unwrap();
    }
    assert!(receiver.take_decodable(50_000).unwrap().is_none());
    for packet in &first[1..] {
        receiver.receive(Channel::Video, packet, 60_000).unwrap();
    }
    let reference = receiver.take_decodable(60_000).unwrap().unwrap();
    assert!(!reference.within_display_queue_budget());
    assert_eq!(reference.descriptor().frame, 1);
    receiver
        .acknowledge_decode(&reference, true, 61_000)
        .unwrap();
    drop(reference);
    let latest = receiver.take_decodable(61_000).unwrap().unwrap();
    assert!(latest.within_display_queue_budget());
    assert_eq!(latest.descriptor().frame, 2);
}
#[test]
fn reliable_announcement_detects_an_entirely_lost_final_frame() {
    let c = config();
    let (mut receiver, _) = running(c);
    let d = descriptor(1, Some(0));
    let announcement = progress(d, c);
    receiver
        .receive(Channel::MediaConfig, &announcement, 100)
        .unwrap();
    assert_eq!(receiver.next_deadline(), Some(20_100));
    let mut packet = [0; 1_150];
    assert_eq!(receiver.repair_request(20_099, &mut packet), Ok(None));
    let n = receiver
        .repair_request(20_100, &mut packet)
        .unwrap()
        .unwrap();
    let request = decode_repair(
        Record::decode(
            &packet[..n],
            &c.limits,
            c.bindings.for_channel(Channel::Control),
            Channel::Control,
        )
        .unwrap(),
        3,
        &c.limits,
    )
    .unwrap();
    assert_eq!(request.frame, 1);
    assert_eq!(
        request.ranges().collect::<Vec<_>>(),
        [RepairRange { start: 0, end: 3 }]
    );
    assert_eq!(receiver.repair_request(20_101, &mut packet), Ok(None));
    receiver
        .receive(Channel::MediaConfig, &announcement, 240_000)
        .unwrap();
    assert_eq!(receiver.tick(250_100), Err(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
}
#[test]
fn repair_requests_select_only_missing_fragments_and_stop_at_retry_limit() {
    let c = config();
    let (mut receiver, _) = running(c);
    let packets = fragments(descriptor(1, Some(0)), &payload(), c);
    receiver.receive(Channel::Video, &packets[1], 0).unwrap();
    let mut out = [0; 1_150];
    for now in [20_000, 80_000, 140_000] {
        let n = receiver.repair_request(now, &mut out).unwrap().unwrap();
        let request = decode_repair(
            Record::decode(
                &out[..n],
                &c.limits,
                c.bindings.for_channel(Channel::Control),
                Channel::Control,
            )
            .unwrap(),
            3,
            &c.limits,
        )
        .unwrap();
        assert_eq!(
            request.ranges().collect::<Vec<_>>(),
            [
                RepairRange { start: 0, end: 1 },
                RepairRange { start: 2, end: 3 }
            ]
        );
    }
    assert_eq!(receiver.repair_request(200_000, &mut out), Ok(None));
    assert_eq!(receiver.next_deadline(), Some(250_000));
    assert_eq!(receiver.tick(250_000), Err(DeliveryError::ReferenceExpired));
}
#[test]
fn stale_bindings_allocate_nothing_and_do_not_break_current_stream() {
    let c = config();
    let (mut receiver, budget) = running(c);
    let mut packet = fragments(descriptor(1, Some(0)), &payload(), c).remove(0);
    packet[16..20].copy_from_slice(&99_u32.to_be_bytes());
    assert_eq!(
        receiver.receive(Channel::Video, &packet, 1),
        Err(DeliveryError::StaleGeneration)
    );
    assert_eq!(receiver.state(), ReceiveState::Streaming);
    assert_eq!(budget.usage(), BudgetUsage::default());
}
#[test]
fn recovery_refuses_gaps_and_never_exposes_a_partial_idr() {
    let c = config();
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    let mut packet = [0; 1_150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 2_500,
            offset: 1,
            capture_micros: 123,
            bytes: &[1, 2],
        },
        c.bindings.for_channel(Channel::Recovery),
        &c.limits,
        &mut packet,
    )
    .unwrap();
    assert_eq!(
        receiver.receive(Channel::Recovery, &packet[..n], 1),
        Err(DeliveryError::NoncontiguousRecovery)
    );
    assert_eq!(budget.usage(), BudgetUsage::default());
    assert!(receiver.take_decodable(1).is_err());
}
#[test]
fn held_decoder_inputs_remain_charged_after_close_and_generation_replacement() {
    let mut c = config();
    let limits = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(2),
        ..LimitOverrides::default()
    })
    .unwrap();
    c.limits = MediaLimits::new(limits, 1_150, 16_384, 64).unwrap();
    let budget = MediaBudget::new(&limits).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    recovery(&mut receiver, c, 0);
    let old = receiver.take_decodable(1).unwrap().unwrap();
    let old_usage = budget.usage();
    assert_eq!(old_usage.pictures, 1);
    assert!(old_usage.bytes > old.bytes().len());
    let epoch = MediaEpoch {
        recovery: RecoveryGeneration::from_raw(1),
        ..c.epoch
    };
    let bindings = MediaBindings::new(5, 6, 7, 8).unwrap();
    receiver.replace(epoch, bindings, 2).unwrap();
    assert_eq!(budget.usage(), old_usage);
    assert_eq!(
        receiver.acknowledge_decode(&old, true, 2),
        Err(DeliveryError::StaleGeneration)
    );
    receiver.close();
    assert_eq!(budget.usage(), old_usage);
    drop(old);
    assert_eq!(budget.usage(), BudgetUsage::default());
}
#[test]
fn picture_count_includes_completed_but_not_released_decoder_inputs() {
    let mut c = config();
    let p = ProtocolLimits::with_overrides(LimitOverrides {
        reassembly_window_pictures: Some(2),
        ..LimitOverrides::default()
    })
    .unwrap();
    c.limits = MediaLimits::new(p, 1_150, 16_384, 64).unwrap();
    let budget = MediaBudget::new(&p).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    recovery(&mut receiver, c, 0);
    let idr = receiver.take_decodable(0).unwrap().unwrap();
    receiver.acknowledge_decode(&idr, true, 0).unwrap();
    for packet in fragments(descriptor(1, Some(0)), &payload(), c) {
        receiver.receive(Channel::Video, &packet, 1).unwrap();
    }
    let next = receiver.take_decodable(2).unwrap().unwrap();
    receiver.acknowledge_decode(&next, true, 2).unwrap();
    assert_eq!(budget.usage().pictures, 2);
    let packets = fragments(descriptor(2, Some(1)), &payload(), c);
    assert_eq!(
        receiver.receive(Channel::Video, &packets[0], 3),
        Err(DeliveryError::ResourceLimit)
    );
    drop(idr);
    drop(next);
    assert_eq!(budget.usage(), BudgetUsage::default());
}
#[test]
fn decode_failure_never_promotes_dependents_and_idle_timer_ends_recovery() {
    let c = config();
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut receiver = ReceivePipeline::new(c, budget.clone()).unwrap();
    receiver.decoder_configured(0).unwrap();
    assert_eq!(receiver.next_deadline(), Some(2_000_000));
    recovery(&mut receiver, c, 1);
    let idr = receiver.take_decodable(1).unwrap().unwrap();
    assert_eq!(
        receiver.acknowledge_decode(&idr, false, 2),
        Err(DeliveryError::DecodeFailed)
    );
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert_eq!(budget.usage().pictures, 1);
    drop(idr);
    let mut other = ReceivePipeline::new(c, budget).unwrap();
    other.decoder_configured(0).unwrap();
    assert_eq!(other.tick(2_000_000), Err(DeliveryError::RecoveryExpired));
}
#[test]
fn clock_faults_close_and_binding_or_epoch_reuse_cannot_reopen() {
    let c = config();
    let (mut receiver, _) = running(c);
    receiver.tick(100).unwrap();
    assert_eq!(receiver.tick(99), Err(DeliveryError::ClockRegression));
    assert_eq!(receiver.state(), ReceiveState::Closed);
    assert_eq!(
        receiver.replace(c.epoch, c.bindings, 101),
        Err(DeliveryError::WrongState)
    );
    let (mut fresh, _) = running(c);
    assert_eq!(
        fresh.replace(c.epoch, c.bindings, 0),
        Err(DeliveryError::StaleGeneration)
    );
    let budget = MediaBudget::new(c.limits.protocol()).unwrap();
    let mut overflow = ReceivePipeline::new(c, budget).unwrap();
    assert_eq!(
        overflow.decoder_configured(u64::MAX),
        Err(DeliveryError::ClockOverflow)
    );
    assert_eq!(overflow.state(), ReceiveState::AwaitingConfiguration);
}
