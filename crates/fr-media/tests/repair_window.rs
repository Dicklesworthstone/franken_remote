//! Production cache/reassembly/byte codecs with deterministic packet loss and
//! clocks. No transport, native decoder or live-tailnet qualification is implied.
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
fn payload(frame: u64, count: u32) -> Vec<u8> {
    let length = usize::try_from(count * config().limits.fragment_stride()).unwrap();
    let seed = u8::try_from(frame % 256).unwrap();
    (0_u8..=255)
        .cycle()
        .skip(usize::from(seed))
        .take(length)
        .collect()
}
fn push(sender: &mut SendCache, frame: u64, count: u32, now: u64) -> Result<(), SendError> {
    let c = config();
    let bytes = if frame == 0 {
        vec![7; 16]
    } else {
        payload(frame, count)
    };
    sender.push(
        Progress {
            descriptor: FrameDescriptor {
                frame,
                reference: frame.checked_sub(1),
                total_bytes: u32::try_from(bytes.len()).unwrap(),
                stride: c.limits.fragment_stride(),
                capture_micros: now,
            },
            observed_micros: now,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        bytes,
        if frame == 0 {
            DeliveryMode::Recovery
        } else {
            DeliveryMode::Datagrams
        },
        now,
    )
}
fn sender(policy: SendPolicy) -> SendCache {
    let c = config();
    let mut s = SendCache::new(c.limits, c.bindings, c.epoch, policy).unwrap();
    push(&mut s, 0, 1, 0).unwrap();
    originals(&mut s, None, 0, None);
    s
}
fn fragment(bytes: &[u8]) -> (u64, u32) {
    let c = config();
    let record = Record::decode(
        bytes,
        &c.limits,
        c.bindings.for_channel(Channel::Video),
        Channel::Video,
    )
    .unwrap();
    let f = decode_fragment(record, &c.limits).unwrap();
    (f.descriptor.frame, f.index)
}
fn originals(s: &mut SendCache, mut r: Option<&mut ReceivePipeline>, now: u64, lost: Option<u32>) {
    let mut bytes = [0; 1_150];
    while let Some(offer) = s.next_packet(now, &mut bytes).unwrap() {
        s.authorize_write(&offer, now).unwrap();
        let bytes = &bytes[..offer.byte_len()];
        if offer.channel() == Channel::Video && lost == Some(fragment(bytes).1) {
            continue;
        }
        if let Some(r) = r.as_deref_mut() {
            r.receive(offer.channel(), bytes, now).unwrap();
        }
    }
}
fn request(frame: u64, start: u32, end: u32, count: u32) -> Vec<u8> {
    let c = config();
    let mut bytes = vec![0; 1_150];
    let len = encode_repair(
        frame,
        &[RepairRange { start, end }],
        count,
        c.bindings.for_channel(Channel::Control),
        &c.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(len);
    bytes
}
fn repair(s: &mut SendCache, now: u64) -> Option<(u64, u32)> {
    let mut bytes = [0; 1_150];
    let offer = s.next_repair_packet(now, &mut bytes).unwrap()?;
    s.authorize_write(&offer, now).unwrap();
    Some(fragment(&bytes[..offer.byte_len()]))
}
fn populate(s: &mut SendCache, frames: u64, count: u32) {
    for frame in 1..=frames {
        push(s, frame, count, 1_000).unwrap();
        originals(s, None, 1_000, None);
    }
}

#[test]
fn one_receiver_attempt_repairs_each_picture_in_a_burst_of_lost_references() {
    let c = config();
    let mut s = SendCache::new(
        c.limits,
        c.bindings,
        c.epoch,
        SendPolicy {
            max_repair_rounds: 1,
            ..SendPolicy::default()
        },
    )
    .unwrap();
    let mut r = ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    r.decoder_configured(0).unwrap();
    push(&mut s, 0, 1, 0).unwrap();
    originals(&mut s, Some(&mut r), 0, None);
    let boot = r.take_decodable(0).unwrap().unwrap();
    r.acknowledge_decode(&boot, true, 0).unwrap();
    drop(boot);
    // Every picture depends on its predecessor. No duplicate request or new
    // generation can hide a dropped request for any of the four lost prefixes.
    for frame in 1..=4 {
        push(&mut s, frame, 3, 1_000).unwrap();
        originals(&mut s, Some(&mut r), 1_000, Some(0));
    }
    let charged = (s.cached_bytes(), r.budget_usage());
    let mut packet = [0; 1_150];
    for frame in 1..=4 {
        let offer = r.repair_offer(21_000, &mut packet).unwrap().unwrap();
        assert_eq!(offer.frame, frame);
        s.queue_repair(&packet[..offer.bytes], 41_000).unwrap();
    }
    assert!(r.repair_offer(121_000, &mut packet).unwrap().is_none());
    assert_eq!((s.cached_bytes(), r.budget_usage()), charged);
    for frame in 1..=4 {
        let offer = s.next_repair_packet(121_000, &mut packet).unwrap().unwrap();
        assert_eq!(offer.frame(), frame);
        assert_eq!(offer.send_by_micros(), 251_000);
        s.authorize_write(&offer, 121_000).unwrap();
        r.receive(Channel::Video, &packet[..offer.byte_len()], 141_000)
            .unwrap();
        let picture = r.take_decodable(141_000).unwrap().unwrap();
        assert_eq!(picture.descriptor().frame, frame);
        assert_eq!(picture.epoch(), c.epoch);
        assert_eq!(picture.bytes(), payload(frame, 3));
        r.acknowledge_decode(&picture, true, 141_000).unwrap();
    }
    assert_eq!(r.budget_usage().pictures, 0);
    assert!(!s.needs_recovery());
    assert_eq!(repair(&mut s, 141_000), None);
}

#[test]
fn late_oldest_request_preempts_a_partly_emitted_newer_repair_without_losing_it() {
    let mut s = sender(SendPolicy::default());
    populate(&mut s, 3, 3);
    s.queue_repair(&request(3, 0, 3, 3), 20_000).unwrap();
    assert_eq!(repair(&mut s, 20_000), Some((3, 0)));
    s.queue_repair(&request(2, 2, 3, 3), 20_001).unwrap();
    s.queue_repair(&request(1, 1, 3, 3), 20_001).unwrap();
    let actual: Vec<_> = std::iter::from_fn(|| repair(&mut s, 20_001)).collect();
    assert_eq!(actual, [(1, 1), (1, 2), (2, 2), (3, 1), (3, 2)]);
}

#[test]
fn an_unissued_later_tail_does_not_block_a_ready_predecessor_repair() {
    let mut s = sender(SendPolicy::default());
    populate(&mut s, 1, 3);
    push(&mut s, 2, 3, 1_000).unwrap();
    // Only the announcement, not the original data, has left picture 2.
    let offer = s.next_packet(1_000, &mut [0; 1_150]).unwrap().unwrap();
    assert_eq!(offer.channel(), Channel::MediaConfig);
    s.queue_repair(&request(2, 0, 3, 3), 20_000).unwrap();
    s.queue_repair(&request(1, 0, 1, 3), 20_000).unwrap();
    assert_eq!(repair(&mut s, 20_000), Some((1, 0)));
    for now in 20_000..20_100 {
        assert_eq!(repair(&mut s, now), None);
    }
    originals(&mut s, None, 21_000, None);
    for index in 0..3 {
        assert_eq!(repair(&mut s, 21_000), Some((2, index)));
    }
}

#[test]
fn same_picture_duplicates_and_bad_ranges_never_refill_or_replace_jobs() {
    let mut s = sender(SendPolicy {
        max_repair_rounds: 1,
        ..SendPolicy::default()
    });
    populate(&mut s, 2, 3);
    let charged = s.cached_bytes();
    s.queue_repair(&request(1, 1, 2, 3), 20_000).unwrap();
    for now in 20_000..21_000 {
        assert_eq!(
            s.queue_repair(&request(1, 0, 1, 3), now),
            Err(SendError::RepairBusy)
        );
    }
    assert_eq!(
        s.queue_repair(&request(2, 3, 4, 4), 21_000),
        Err(SendError::Wire(WireError::InvalidRanges))
    );
    s.queue_repair(&request(2, 0, 1, 3), 21_000).unwrap();
    assert_eq!(s.cached_bytes(), charged);
    assert_eq!(repair(&mut s, 21_000), Some((1, 1)));
    assert_eq!(repair(&mut s, 21_000), Some((2, 0)));
    assert_eq!(
        s.queue_repair(&request(1, 0, 1, 3), 40_000),
        Err(SendError::RepairRateLimited)
    );
}

#[test]
fn request_storage_is_precharged_and_bounded_by_the_admitted_picture_window() {
    let policy = SendPolicy {
        max_cached_pictures: 4,
        ..SendPolicy::default()
    };
    let mut s = sender(policy);
    let metadata = s.cached_bytes() - 16;
    assert!(
        metadata >= 64 * 8,
        "the complete inline range storage must be charged"
    );
    populate(&mut s, 3, 1);
    let charged = s.cached_bytes();
    assert_eq!(charged, 4 * metadata + 16 + 3 * payload(1, 1).len());
    assert_eq!(push(&mut s, 4, 1, 1_000), Err(SendError::CacheFull));
    for frame in 1..=3 {
        s.queue_repair(&request(frame, 0, 1, 1), 20_000).unwrap();
    }
    assert_eq!(s.cached_bytes(), charged);
    assert_eq!(s.cached_pictures(), 4);
    let mut restricted = sender(SendPolicy {
        max_cached_bytes: charged - 1,
        repair_bytes_per_window: 1_150,
        ..policy
    });
    populate(&mut restricted, 2, 1);
    assert_eq!(
        push(&mut restricted, 3, 1, 1_000),
        Err(SendError::CacheFull)
    );
}

#[test]
fn short_output_does_not_advance_any_job_or_spend_the_shared_byte_budget() {
    let mut s = sender(SendPolicy {
        repair_bytes_per_window: 2_300,
        ..SendPolicy::default()
    });
    populate(&mut s, 2, 1);
    for frame in [2, 1] {
        s.queue_repair(&request(frame, 0, 1, 1), 20_000).unwrap();
    }
    for _ in 0..10 {
        assert!(matches!(
            s.next_repair_packet(20_000, &mut [0; 10]),
            Err(SendError::Wire(_))
        ));
    }
    assert_eq!(repair(&mut s, 20_000), Some((1, 0)));
    assert_eq!(repair(&mut s, 20_000), Some((2, 0)));
    assert_eq!(repair(&mut s, 20_000), None);
}

#[test]
fn completed_picture_eviction_and_generation_replacement_retire_only_original_jobs() {
    let c = config();
    let mut s = sender(SendPolicy::default());
    push(&mut s, 1, 1, 1).unwrap();
    originals(&mut s, None, 1, None);
    push(&mut s, 2, 1, 100_000).unwrap();
    originals(&mut s, None, 100_000, None);
    for frame in 1..=2 {
        s.queue_repair(&request(frame, 0, 1, 1), 100_001).unwrap();
    }
    s.tick(250_001).unwrap();
    assert_eq!(s.cached_pictures(), 2); // startup and the later picture
    let old = s
        .next_repair_packet(250_001, &mut [0; 1_150])
        .unwrap()
        .unwrap();
    assert_eq!(old.frame(), 2);
    s.replace(
        MediaEpoch {
            recovery: c.epoch.recovery.next().unwrap(),
            ..c.epoch
        },
        MediaBindings::new(5, 6, 7, 8).unwrap(),
        250_002,
    )
    .unwrap();
    assert_eq!(s.cached_bytes(), 0);
    assert_eq!(s.next_repair_packet(250_002, &mut [0; 1_150]), Ok(None));
    assert_eq!(
        s.authorize_write(&old, 250_002),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
}
