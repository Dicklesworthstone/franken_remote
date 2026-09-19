//! Production packetizers/receivers with opaque payloads. Decoder completions
//! here are simulated; this is shared-storage/loss isolation, not HEVC proof.
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::{
    access_unit::{EncodedAccessUnit, FrameId, FrameKind},
    delivery::*,
};
use fr_wire::{Channel, MediaLimits, Record, decode_progress};

fn limits() -> MediaLimits {
    MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap()
}
fn epoch() -> MediaEpoch {
    MediaEpoch {
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn pool(bytes: usize, pictures: usize) -> SharedFramePool {
    SharedFramePool::new(ProtocolLimits::ABSOLUTE, bytes, pictures).unwrap()
}
fn unit(frame: u64, reference: Option<u64>, now: u64, bytes: Vec<u8>) -> EncodedAccessUnit {
    EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::from_raw(frame),
        match reference {
            Some(r) => FrameKind::Predicted {
                references: FrameId::from_raw(r),
            },
            None => FrameKind::Idr {
                recovery: RecoveryGeneration::INITIAL,
            },
        },
        epoch().configuration,
        now,
        bytes,
    )
    .unwrap()
}
fn sender(limits: MediaLimits, bindings: MediaBindings, policy: SendPolicy) -> SendCache {
    SendCache::new(limits, bindings, epoch(), policy).unwrap()
}
fn bindings() -> MediaBindings {
    MediaBindings::new(1, 2, 3, 4).unwrap()
}
fn receiver(limits: MediaLimits, bindings: MediaBindings) -> ReceivePipeline {
    ReceivePipeline::new(
        ReceiveConfig {
            limits,
            bindings,
            epoch: epoch(),
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap()
}
fn drain(s: &mut SendCache, r: &mut ReceivePipeline, now: u64) {
    let mut out = vec![0; 2048];
    while let Some(packet) = s.next_packet(now, &mut out).unwrap() {
        s.authorize_write(&packet, now).unwrap();
        r.receive(packet.channel(), &out[..packet.byte_len()], now)
            .unwrap();
    }
    let picture = r.take_decodable(now).unwrap().unwrap();
    r.complete_decode(&picture, now).unwrap();
}
#[test]
fn two_senders_retain_one_physical_buffer_and_charge_each_viewer() {
    let pool = pool(32_000, 8);
    let bytes = vec![17; 3000];
    let ptr = bytes.as_ptr();
    let frame = pool.share(unit(0, None, 0, bytes)).unwrap();
    assert_eq!(frame.bytes().as_ptr(), ptr);
    let alias = frame.clone();
    assert!(frame.shares_storage_with(&alias));
    let physical = pool.usage();
    assert_eq!(physical.pictures, 1);
    assert!(physical.bytes > 3000);
    let mut a = sender(limits(), bindings(), SendPolicy::default());
    let mut b = sender(limits(), bindings(), SendPolicy::default());
    a.push_shared(&frame, DeliveryMode::Recovery, 0).unwrap();
    b.push_shared(&frame, DeliveryMode::Recovery, 0).unwrap();
    assert_eq!(pool.usage(), physical);
    assert!(a.cached_bytes() > physical.bytes);
    assert_eq!(a.cached_bytes(), b.cached_bytes());
    drop(frame);
    drop(alias);
    a.close();
    assert_eq!(pool.usage(), physical);
    b.close();
    assert_eq!(pool.usage(), BudgetUsage::default());
}
#[test]
fn actual_capacity_counts_and_pool_clones_do_not_refill_credit() {
    let pool = pool(4096, 2);
    let mut bytes = Vec::with_capacity(8192);
    bytes.push(7);
    assert!(matches!(
        pool.share(unit(0, None, 0, bytes)),
        Err(DeliveryError::ResourceLimit)
    ));
    assert_eq!(pool.usage(), BudgetUsage::default());
    let frame = pool.share(unit(0, None, 0, vec![7; 2000])).unwrap();
    let used = pool.usage();
    assert!(!pool.clone().can_share_capacity(2000));
    assert!(
        pool.clone()
            .share(unit(1, Some(0), 0, vec![7; 2000]))
            .is_err()
    );
    assert_eq!(pool.usage(), used);
    drop(frame);
    assert!(pool.can_share_capacity(2000));
    assert!(pool.share(unit(1, Some(0), 0, vec![7; 2000])).is_ok());
}
#[test]
fn picture_count_and_external_owners_survive_cache_eviction() {
    let pool = pool(32_000, 1);
    let frame = pool.share(unit(0, None, 0, vec![7; 3000])).unwrap();
    let mut tx = sender(limits(), bindings(), SendPolicy::default());
    tx.push_shared(&frame, DeliveryMode::Recovery, 0).unwrap();
    let mut out = [0; 1150];
    while tx.next_packet(0, &mut out).unwrap().is_some() {}
    tx.tick(2_000_000).unwrap();
    assert_eq!(tx.cached_pictures(), 0);
    assert_eq!(pool.usage().pictures, 1);
    assert!(!pool.can_share_capacity(1));
    drop(frame);
    assert_eq!(pool.usage(), BudgetUsage::default());
}
#[test]
fn different_datagram_strides_use_the_same_immutable_picture() {
    let pool = pool(32_000, 8);
    let frame = pool.share(unit(0, None, 0, vec![7; 3000])).unwrap();
    let other = MediaLimits::new(ProtocolLimits::ABSOLUTE, 700, 16384, 64).unwrap();
    for selected in [limits(), other] {
        let mut tx = sender(selected, bindings(), SendPolicy::default());
        let mut rx = receiver(selected, bindings());
        rx.decoder_configured(0).unwrap();
        tx.push_shared(&frame, DeliveryMode::Recovery, 0).unwrap();
        drain(&mut tx, &mut rx, 0);
        let delta = pool.share(unit(1, Some(0), 10, vec![8; 3000])).unwrap();
        tx.push_shared(&delta, DeliveryMode::Datagrams, 10).unwrap();
        let mut bytes = [0; 1150];
        let p = tx.next_packet(10, &mut bytes).unwrap().unwrap();
        assert_eq!(p.channel(), Channel::MediaConfig);
        let progress = decode_progress(
            Record::decode(&bytes[..p.byte_len()], &selected, 3, Channel::MediaConfig).unwrap(),
            &selected,
        )
        .unwrap();
        assert_eq!(progress.descriptor.stride, selected.fragment_stride());
        rx.receive(p.channel(), &bytes[..p.byte_len()], 10).unwrap();
        drain(&mut tx, &mut rx, 10);
        assert_eq!(rx.state(), ReceiveState::Streaming);
    }
    assert_eq!(pool.usage().pictures, 1);
}
#[test]
fn closing_or_stalling_one_viewer_does_not_discard_another_reference_chain() {
    let pool = pool(32_000, 8);
    let mut bad = sender(limits(), bindings(), SendPolicy::default());
    let mut good = sender(limits(), bindings(), SendPolicy::default());
    let mut rx = receiver(limits(), bindings());
    rx.decoder_configured(0).unwrap();
    let frame = pool.share(unit(0, None, 0, vec![7; 3000])).unwrap();
    bad.push_shared(&frame, DeliveryMode::Recovery, 0).unwrap();
    good.push_shared(&frame, DeliveryMode::Recovery, 0).unwrap();
    drain(&mut good, &mut rx, 0);
    drop(frame);
    let frame = pool.share(unit(1, Some(0), 10, vec![8; 3000])).unwrap();
    good.push_shared(&frame, DeliveryMode::Datagrams, 10)
        .unwrap();
    bad.push_shared(&frame, DeliveryMode::Datagrams, 10)
        .unwrap();
    drain(&mut good, &mut rx, 10);
    drop(frame);
    assert_eq!(bad.tick(250_010), Err(SendError::OriginalExpired));
    assert!(bad.needs_recovery());
    assert!(!good.needs_recovery());
    let frame = pool
        .share(unit(2, Some(1), 250_010, vec![9; 3000]))
        .unwrap();
    good.push_shared(&frame, DeliveryMode::Datagrams, 250_010)
        .unwrap();
    drain(&mut good, &mut rx, 250_010);
    assert_eq!(rx.state(), ReceiveState::Streaming);
    assert_eq!(bad.cached_bytes(), 0);
    drop(frame);
    good.close();
    assert_eq!(pool.usage(), BudgetUsage::default());
}
#[test]
fn a_shared_idr_recovers_one_viewer_without_resetting_a_healthy_viewer() {
    let pool = pool(32_000, 8);
    let mut a = sender(limits(), bindings(), SendPolicy::default());
    let mut b = sender(limits(), bindings(), SendPolicy::default());
    let mut ar = receiver(limits(), bindings());
    let mut br = receiver(limits(), bindings());
    ar.decoder_configured(0).unwrap();
    br.decoder_configured(0).unwrap();
    let initial = pool.share(unit(0, None, 0, vec![7; 3000])).unwrap();
    a.push_shared(&initial, DeliveryMode::Recovery, 0).unwrap();
    b.push_shared(&initial, DeliveryMode::Recovery, 0).unwrap();
    drain(&mut a, &mut ar, 0);
    drain(&mut b, &mut br, 0);
    drop(initial);
    let fresh = MediaEpoch {
        recovery: epoch().recovery.next().unwrap(),
        ..epoch()
    };
    let routes = MediaBindings::new(11, 12, 13, 14).unwrap();
    a.replace(fresh, routes, 10).unwrap();
    ar.replace(fresh, routes, 10).unwrap();
    ar.decoder_configured(10).unwrap();
    let idr = pool.share(unit(5, None, 10, vec![8; 3000])).unwrap();
    a.push_shared(&idr, DeliveryMode::Recovery, 10).unwrap();
    b.push_shared(&idr, DeliveryMode::Datagrams, 10).unwrap();
    drain(&mut a, &mut ar, 10);
    drain(&mut b, &mut br, 10);
    drop(idr);
    let delta = pool.share(unit(6, Some(5), 20, vec![9; 3000])).unwrap();
    a.push_shared(&delta, DeliveryMode::Datagrams, 20).unwrap();
    b.push_shared(&delta, DeliveryMode::Datagrams, 20).unwrap();
    drain(&mut a, &mut ar, 20);
    drain(&mut b, &mut br, 20);
    assert_eq!(ar.state(), ReceiveState::Streaming);
    assert_eq!(br.state(), ReceiveState::Streaming);
    // Both sent different generation/channel bindings from the same storage.
    assert!(b.next_packet(20, &mut [0; 1150]).unwrap().is_none());
}
#[test]
fn sharing_never_renews_capture_age_or_bypasses_per_viewer_credit() {
    let pool = pool(32_000, 8);
    let frame = pool.share(unit(0, None, 0, vec![7; 3000])).unwrap();
    let mut late = sender(limits(), bindings(), SendPolicy::default());
    assert_eq!(
        late.push_shared(&frame, DeliveryMode::Recovery, 2_000_000),
        Err(SendError::OriginalExpired)
    );
    let mut small = sender(
        limits(),
        bindings(),
        SendPolicy {
            max_cached_bytes: 3100,
            repair_bytes_per_window: 1000,
            ..SendPolicy::default()
        },
    );
    assert_eq!(
        small.push_shared(&frame, DeliveryMode::Recovery, 0),
        Err(SendError::CacheFull)
    );
    assert_eq!(small.cached_bytes(), 0);
    assert_eq!(pool.usage().pictures, 1);
}
#[test]
fn shared_configuration_and_dependencies_cannot_be_relabelled() {
    let pool = pool(32_000, 8);
    let mut tx = sender(limits(), bindings(), SendPolicy::default());
    let wrong = EncodedAccessUnit::new(
        &ProtocolLimits::ABSOLUTE,
        FrameId::FIRST,
        FrameKind::Idr {
            recovery: epoch().recovery,
        },
        epoch().configuration.next().unwrap(),
        0,
        vec![7; 3000],
    )
    .unwrap();
    let wrong = pool.share(wrong).unwrap();
    assert_eq!(
        tx.push_shared(&wrong, DeliveryMode::Recovery, 0),
        Err(SendError::Delivery(DeliveryError::StaleGeneration))
    );
    let frame = pool.share(unit(0, None, 0, vec![7; 3000])).unwrap();
    tx.push_shared(&frame, DeliveryMode::Recovery, 0).unwrap();
    let frame = pool.share(unit(2, Some(1), 10, vec![8; 3000])).unwrap();
    assert_eq!(
        tx.push_shared(&frame, DeliveryMode::Datagrams, 10),
        Err(SendError::InvalidDependency)
    );
    assert_eq!(tx.cached_pictures(), 1);
    assert!(!format!("{frame:?}").contains("[8, 8"));
}
#[test]
fn pool_policy_cannot_expand_the_configured_ceiling() {
    for (bytes, count) in [(0, 1), (32_000, 0), (32_000, 65), (usize::MAX, 1)] {
        assert!(matches!(
            SharedFramePool::new(ProtocolLimits::ABSOLUTE, bytes, count),
            Err(DeliveryError::InvalidPolicy)
        ));
    }
}

#[test]
fn selective_repair_retains_the_original_shared_picture_and_deadline() {
    use fr_wire::{RepairRange, encode_repair};
    let pool = pool(32_000, 8);
    let mut tx = sender(limits(), bindings(), SendPolicy::default());
    let mut rx = receiver(limits(), bindings());
    rx.decoder_configured(0).unwrap();
    let initial = pool.share(unit(0, None, 0, vec![7; 3000])).unwrap();
    tx.push_shared(&initial, DeliveryMode::Recovery, 0).unwrap();
    drain(&mut tx, &mut rx, 0);
    drop(initial);
    let frame = pool.share(unit(1, Some(0), 10, vec![9; 3000])).unwrap();
    tx.push_shared(&frame, DeliveryMode::Datagrams, 10).unwrap();
    drop(frame);
    let used = pool.usage();
    let mut bytes = [0; 1150];
    let mut lost = None;
    while let Some(packet) = tx.next_packet(10, &mut bytes).unwrap() {
        if packet.channel() == Channel::Video && lost.is_none() {
            lost = Some(bytes[..packet.byte_len()].to_vec());
        } else {
            rx.receive(packet.channel(), &bytes[..packet.byte_len()], 10)
                .unwrap();
        }
    }
    assert!(rx.take_decodable(10).unwrap().is_none());
    let mut repair = [0; 1150];
    let n = encode_repair(
        1,
        &[RepairRange { start: 0, end: 1 }],
        3,
        4,
        &limits(),
        &mut repair,
    )
    .unwrap();
    tx.queue_repair(&repair[..n], 20).unwrap();
    let packet = tx.next_repair_packet(20, &mut bytes).unwrap().unwrap();
    assert_eq!(&bytes[..packet.byte_len()], lost.unwrap());
    assert_eq!(packet.send_by_micros(), 250_010);
    assert_eq!(pool.usage(), used);
    rx.receive(packet.channel(), &bytes[..packet.byte_len()], 20)
        .unwrap();
    let picture = rx.take_decodable(20).unwrap().unwrap();
    rx.complete_decode(&picture, 20).unwrap();
    tx.close();
    assert_eq!(pool.usage(), BudgetUsage::default());
}
