//! Production sender/wire/receiver integration under deterministic packet loss.
//! These byte-pattern tests do not claim native network or HEVC qualification.
use fr_core::ids::{CodecConfigurationGeneration, RecoveryGeneration};
use fr_core::limits::ProtocolLimits;
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
fn progress(frame: u64, size: usize, now: u64) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            total_bytes: u32::try_from(size).unwrap(),
            stride: 1_077,
            capture_micros: now,
            reference: frame.checked_sub(1),
        },
        observed_micros: now,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}
fn sender(c: ReceiveConfig, policy: SendPolicy) -> SendCache {
    SendCache::new(c.limits, c.bindings, c.epoch, policy).unwrap()
}
fn originals(cache: &mut SendCache, now: u64) -> Vec<(PacketOffer, Vec<u8>)> {
    let mut packets = Vec::new();
    let mut out = [0; 1_150];
    while let Some(offer) = cache.next_packet(now, &mut out).unwrap() {
        let bytes = out[..offer.byte_len()].to_vec();
        packets.push((offer, bytes));
    }
    packets
}
fn bootstrap(c: ReceiveConfig, policy: SendPolicy) -> (SendCache, ReceivePipeline) {
    let mut sender = sender(c, policy);
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    sender
        .push(
            progress(0, 3_000, 0),
            vec![9; 3_000],
            DeliveryMode::Recovery,
            0,
        )
        .unwrap();
    let packets = originals(&mut sender, 0);
    assert_eq!(packets[0].0.channel(), Channel::MediaConfig);
    for (offer, bytes) in packets {
        receiver.receive(offer.channel(), &bytes, 0).unwrap();
    }
    let picture = receiver.take_decodable(0).unwrap().unwrap();
    assert_eq!(picture.bytes(), vec![9; 3_000]);
    receiver.acknowledge_decode(&picture, true, 0).unwrap();
    (sender, receiver)
}
fn repair_packet(frame: u64, start: u32, end: u32, count: u32, c: ReceiveConfig) -> Vec<u8> {
    let mut out = vec![0; 1_150];
    let n = encode_repair(
        frame,
        &[RepairRange { start, end }],
        count,
        c.bindings.for_channel(Channel::Control),
        &c.limits,
        &mut out,
    )
    .unwrap();
    out.truncate(n);
    out
}

#[test]
fn sender_receiver_roundtrip_recovers_reordered_dropped_and_final_frame_packets() {
    let c = config();
    let (mut sender, mut receiver) = bootstrap(c, SendPolicy::default());
    let mut out = [0; 1_150];
    let mut repairs = 0;
    for frame in 1_u64..=40 {
        let now = frame * 50_000;
        let size = 4_000 + usize::try_from(frame).unwrap() * 113;
        let bytes: Vec<u8> = (0_u8..=255)
            .cycle()
            .skip(usize::try_from(frame).unwrap())
            .take(size)
            .collect();
        sender
            .push(
                progress(frame, size, now),
                bytes.clone(),
                DeliveryMode::Datagrams,
                now,
            )
            .unwrap();
        let packets = originals(&mut sender, now);
        for (index, (offer, packet)) in packets.iter().enumerate().rev() {
            // All final-picture datagrams are lost; reliable progress remains.
            if offer.channel() == Channel::Video && (frame == 40 || index % 3 == 0) {
                continue;
            }
            receiver.receive(offer.channel(), packet, now).unwrap();
            if offer.channel() == Channel::Video {
                receiver.receive(offer.channel(), packet, now).unwrap();
            }
        }
        assert!(receiver.take_decodable(now).unwrap().is_none());
        let request_len = receiver
            .repair_request(now + 20_000, &mut out)
            .unwrap()
            .unwrap();
        sender
            .queue_repair(&out[..request_len], now + 20_000)
            .unwrap();
        while let Some(offer) = sender.next_repair_packet(now + 30_000, &mut out).unwrap() {
            assert!(offer.send_by_micros() > now + 30_000);
            receiver
                .receive(offer.channel(), &out[..offer.byte_len()], now + 30_000)
                .unwrap();
            repairs += 1;
        }
        let picture = receiver.take_decodable(now + 31_000).unwrap().unwrap();
        assert_eq!(picture.descriptor().frame, frame);
        assert_eq!(picture.bytes(), bytes);
        receiver
            .acknowledge_decode(&picture, true, now + 31_000)
            .unwrap();
        drop(picture);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    }
    assert!(repairs > 40);
    sender.tick(3_000_000).unwrap();
    assert_eq!(sender.cached_bytes(), 0);
}
#[test]
fn repairing_an_older_reference_unblocks_two_complete_pictures_in_order() {
    let c = config();
    let (mut sender, mut receiver) = bootstrap(c, SendPolicy::default());
    let first = progress(1, 3_000, 10);
    let second = progress(2, 3_000, 20);
    sender
        .push(first, vec![1; 3_000], DeliveryMode::Datagrams, 10)
        .unwrap();
    for (offer, packet) in originals(&mut sender, 10) {
        if offer.channel() == Channel::MediaConfig {
            receiver.receive(offer.channel(), &packet, 10).unwrap();
        }
    }
    sender
        .push(second, vec![2; 3_000], DeliveryMode::Datagrams, 20)
        .unwrap();
    for (offer, packet) in originals(&mut sender, 20) {
        receiver.receive(offer.channel(), &packet, 20).unwrap();
    }
    assert!(receiver.take_decodable(20).unwrap().is_none());
    let mut out = [0; 1_150];
    let n = receiver.repair_request(60_000, &mut out).unwrap().unwrap();
    sender.queue_repair(&out[..n], 60_000).unwrap();
    while let Some(offer) = sender.next_repair_packet(70_000, &mut out).unwrap() {
        receiver
            .receive(offer.channel(), &out[..offer.byte_len()], 70_000)
            .unwrap();
    }
    for frame in [1_u64, 2] {
        let picture = receiver.take_decodable(70_000).unwrap().unwrap();
        assert_eq!(picture.descriptor().frame, frame);
        receiver.acknowledge_decode(&picture, true, 70_000).unwrap();
    }
}
#[test]
fn sender_cache_is_not_limited_to_twelve_pictures_at_sixty_fps() {
    let c = config();
    let mut sender = sender(c, SendPolicy::default());
    for frame in 0_u64..180 {
        let now = frame * 16_667;
        sender
            .push(
                progress(frame, 100, now),
                vec![5; 100],
                if frame == 0 {
                    DeliveryMode::Recovery
                } else {
                    DeliveryMode::Datagrams
                },
                now,
            )
            .unwrap();
        assert_ne!(originals(&mut sender, now).len(), 0);
        assert!(sender.cached_pictures() <= 17);
    }
    assert!(sender.cached_pictures() > 12);
}
#[test]
fn repair_ranges_are_checked_against_the_actual_cached_picture() {
    let c = config();
    let (mut sender, _) = bootstrap(c, SendPolicy::default());
    sender
        .push(
            progress(1, 3_000, 1),
            vec![7; 3_000],
            DeliveryMode::Datagrams,
            1,
        )
        .unwrap();
    originals(&mut sender, 1);
    let forged = repair_packet(1, 0, 4, 4, c);
    assert_eq!(
        sender.queue_repair(&forged, 20_000),
        Err(SendError::Wire(WireError::InvalidRanges))
    );
    let valid = repair_packet(1, 1, 2, 3, c);
    sender.queue_repair(&valid, 20_000).unwrap();
    assert_eq!(
        sender.queue_repair(&valid, 20_001),
        Err(SendError::RepairBusy)
    );
    let mut out = [0; 1_150];
    let offer = sender
        .next_repair_packet(20_001, &mut out)
        .unwrap()
        .unwrap();
    let fragment = decode_fragment(
        Record::decode(
            &out[..offer.byte_len()],
            &c.limits,
            c.bindings.for_channel(Channel::Video),
            Channel::Video,
        )
        .unwrap(),
        &c.limits,
    )
    .unwrap();
    assert_eq!(fragment.index, 1);
    assert_eq!(fragment.descriptor.frame, 1);
    assert_eq!(sender.next_repair_packet(20_001, &mut out), Ok(None));
    assert_eq!(
        sender.queue_repair(&valid, 20_002),
        Err(SendError::RepairRateLimited)
    );
}
#[test]
fn repair_lifetime_never_extends_and_expired_ids_cannot_be_reinserted() {
    let c = config();
    let (mut sender, _) = bootstrap(c, SendPolicy::default());
    let p = progress(1, 3_000, 1);
    sender
        .push(p, vec![7; 3_000], DeliveryMode::Datagrams, 1)
        .unwrap();
    originals(&mut sender, 1);
    let request = repair_packet(1, 0, 3, 3, c);
    sender.queue_repair(&request, 249_999).unwrap();
    assert_eq!(
        sender.next_repair_packet(250_001, &mut [0; 1_150]),
        Ok(None)
    );
    assert_eq!(
        sender.queue_repair(&request, 250_001),
        Err(SendError::FrameUnavailable)
    );
    assert_eq!(
        sender.push(p, vec![7; 3_000], DeliveryMode::Datagrams, 250_001),
        Err(SendError::InvalidSequence)
    );
}
#[test]
fn expiry_of_an_unsent_original_fences_its_dependents() {
    let c = config();
    let (mut sender, _) = bootstrap(c, SendPolicy::default());
    sender
        .push(
            progress(1, 3_000, 10),
            vec![1; 3_000],
            DeliveryMode::Datagrams,
            10,
        )
        .unwrap();
    sender
        .push(
            progress(2, 3_000, 20),
            vec![2; 3_000],
            DeliveryMode::Datagrams,
            20,
        )
        .unwrap();
    assert_eq!(
        sender.next_packet(250_010, &mut [0; 1_150]),
        Err(SendError::OriginalExpired)
    );
    assert!(sender.needs_recovery());
    assert_eq!(sender.cached_bytes(), 0);
    assert_eq!(
        sender.next_packet(250_011, &mut [0; 1_150]),
        Err(SendError::NeedsRecovery)
    );
}
#[test]
fn actual_vector_capacity_and_picture_count_are_admitted() {
    let c = config();
    let policy = SendPolicy {
        max_cached_bytes: 4_096,
        repair_bytes_per_window: 4_096,
        max_cached_pictures: 1,
        ..SendPolicy::default()
    };
    let mut sender = sender(c, policy);
    let mut overallocated = Vec::with_capacity(8_192);
    overallocated.push(9);
    assert_eq!(
        sender.push(progress(0, 1, 0), overallocated, DeliveryMode::Recovery, 0),
        Err(SendError::CacheFull)
    );
    assert_eq!(sender.cached_bytes(), 0);
    sender
        .push(progress(0, 1, 0), vec![9], DeliveryMode::Recovery, 0)
        .unwrap();
    assert_eq!(
        sender.push(progress(1, 1, 1), vec![9], DeliveryMode::Datagrams, 1),
        Err(SendError::CacheFull)
    );
    assert!(sender.cached_bytes() > 1);
}
#[test]
fn repair_byte_budget_survives_recovery_generation_replacement() {
    let c = config();
    let policy = SendPolicy {
        repair_bytes_per_window: 1_150,
        ..SendPolicy::default()
    };
    let (mut sender, _) = bootstrap(c, policy);
    sender
        .push(
            progress(1, 3_000, 1),
            vec![1; 3_000],
            DeliveryMode::Datagrams,
            1,
        )
        .unwrap();
    originals(&mut sender, 1);
    let request = repair_packet(1, 0, 3, 3, c);
    sender.queue_repair(&request, 20_000).unwrap();
    let mut out = [0; 1_150];
    assert_eq!(
        sender
            .next_repair_packet(20_000, &mut out)
            .unwrap()
            .unwrap()
            .byte_len(),
        1_150
    );
    assert_eq!(
        sender.next_repair_packet(20_001, &mut out),
        Err(SendError::RepairBudgetExceeded)
    );
    let mut next = c;
    next.epoch.recovery = RecoveryGeneration::from_raw(1);
    next.bindings = MediaBindings::new(5, 6, 7, 8).unwrap();
    sender.replace(next.epoch, next.bindings, 30_000).unwrap();
    sender
        .push(
            progress(0, 3_000, 30_000),
            vec![0; 3_000],
            DeliveryMode::Recovery,
            30_000,
        )
        .unwrap();
    originals(&mut sender, 30_000);
    sender
        .push(
            progress(1, 3_000, 30_001),
            vec![1; 3_000],
            DeliveryMode::Datagrams,
            30_001,
        )
        .unwrap();
    originals(&mut sender, 30_001);
    assert_eq!(
        sender.queue_repair(&request, 40_000),
        Err(SendError::Wire(WireError::InvalidBinding))
    );
    sender
        .queue_repair(&repair_packet(1, 0, 3, 3, next), 40_000)
        .unwrap();
    assert_eq!(
        sender.next_repair_packet(40_000, &mut out),
        Err(SendError::RepairBudgetExceeded)
    );
}
#[test]
fn a_too_small_output_buffer_does_not_consume_an_original_packet() {
    let c = config();
    let mut sender = sender(c, SendPolicy::default());
    sender
        .push(
            progress(0, 3_000, 0),
            vec![9; 3_000],
            DeliveryMode::Recovery,
            0,
        )
        .unwrap();
    assert_eq!(
        sender.next_packet(0, &mut [0; 1]),
        Err(SendError::Wire(WireError::BufferTooSmall))
    );
    let offer = sender.next_packet(0, &mut [0; 1_150]).unwrap().unwrap();
    assert_eq!(offer.channel(), Channel::MediaConfig);
}

#[test]
fn final_write_rejects_foreign_and_replaced_original_and_repair_offers() {
    let c = config();
    let (mut cache, _) = bootstrap(c, SendPolicy::default());
    cache
        .push(
            progress(1, 3_000, 1),
            vec![1; 3_000],
            DeliveryMode::Datagrams,
            1,
        )
        .unwrap();
    let packets = originals(&mut cache, 1);
    let original = &packets[1].0;
    cache.authorize_write(original, 1).unwrap();
    let (mut foreign, _) = bootstrap(c, SendPolicy::default());
    assert_eq!(
        foreign.authorize_write(original, 1),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
    cache
        .queue_repair(&repair_packet(1, 0, 1, 3, c), 20_000)
        .unwrap();
    let repair = cache
        .next_repair_packet(20_000, &mut [0; 1_150])
        .unwrap()
        .unwrap();
    cache.authorize_write(&repair, 20_000).unwrap();
    let epoch = MediaEpoch {
        recovery: RecoveryGeneration::from_raw(1),
        ..c.epoch
    };
    // A refused replacement leaves the current cache and offers usable.
    assert_eq!(
        cache.replace(epoch, MediaBindings::new(4, 5, 6, 7).unwrap(), 20_001),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
    cache.authorize_write(original, 20_001).unwrap();
    cache
        .replace(epoch, MediaBindings::new(5, 6, 7, 8).unwrap(), 20_002)
        .unwrap();
    assert_eq!(cache.cached_bytes(), 0);
    for offer in [original, &repair] {
        assert_eq!(
            cache.authorize_write(offer, 20_002),
            Err(SendError::Delivery(DeliveryError::StaleGeneration))
        );
    }
    cache
        .push(
            progress(0, 3_000, 20_002),
            vec![2; 3_000],
            DeliveryMode::Recovery,
            20_002,
        )
        .unwrap();
    let fresh = cache.next_packet(20_002, &mut [0; 1_150]).unwrap().unwrap();
    cache.authorize_write(&fresh, 20_002).unwrap();
    cache.close();
    assert_eq!(
        cache.authorize_write(&fresh, 20_003),
        Err(SendError::Closed)
    );
}

#[test]
fn final_write_services_unsent_predecessor_expiry_and_exact_deadline() {
    let c = config();
    // All original chunks were prepared: cache expiry alone does not fence the
    // chain, but the retained offer still ends at its exact exclusive deadline.
    let mut complete = sender(c, SendPolicy::default());
    complete
        .push(progress(0, 100, 0), vec![0; 100], DeliveryMode::Recovery, 0)
        .unwrap();
    let packets = originals(&mut complete, 0);
    let offered = &packets[1].0;
    complete.authorize_write(offered, 1_999_999).unwrap();
    assert_eq!(
        complete.authorize_write(offered, 2_000_000),
        Err(SendError::OriginalExpired)
    );
    assert!(!complete.needs_recovery());
    // Partly unsent originals instead fence the whole chain on expiry.
    let (mut fresh_cache, _) = bootstrap(c, SendPolicy::default());
    fresh_cache
        .push(
            progress(1, 3_000, 1),
            vec![1; 3_000],
            DeliveryMode::Datagrams,
            1,
        )
        .unwrap();
    let offer = fresh_cache
        .next_packet(1, &mut [0; 1_150])
        .unwrap()
        .unwrap();
    assert_eq!(
        fresh_cache.authorize_write(&offer, 250_001),
        Err(SendError::OriginalExpired)
    );
    assert!(fresh_cache.needs_recovery());
    // Retain a fully prepared recovery offer, then leave the next P unit unsent.
    let mut early = sender(c, SendPolicy::default());
    early
        .push(progress(0, 100, 0), vec![0; 100], DeliveryMode::Recovery, 0)
        .unwrap();
    let packets = originals(&mut early, 0);
    let old = &packets[1].0;
    early
        .push(
            progress(1, 3_000, 1),
            vec![1; 3_000],
            DeliveryMode::Datagrams,
            1,
        )
        .unwrap();
    assert!(old.send_by_micros() > 250_001);
    assert_eq!(
        early.authorize_write(old, 250_001),
        Err(SendError::OriginalExpired)
    );
    assert_eq!(early.cached_bytes(), 0);
}
#[test]
fn bad_reference_and_counter_wrap_cannot_create_an_ambiguous_chain() {
    let c = config();
    let (mut sender, _) = bootstrap(c, SendPolicy::default());
    assert_eq!(
        sender.push(progress(2, 1, 1), vec![1], DeliveryMode::Datagrams, 1),
        Err(SendError::InvalidDependency)
    );
    let mut last = progress(u64::MAX, 1, 2);
    last.descriptor.reference = Some(0);
    sender
        .push(last, vec![1], DeliveryMode::Datagrams, 2)
        .unwrap();
    originals(&mut sender, 2);
    assert_eq!(
        sender.push(progress(1, 1, 3), vec![1], DeliveryMode::Datagrams, 3),
        Err(SendError::InvalidSequence)
    );
}
