//! Real sender, receiver and wire owners with explicit synthetic compressed bytes
//! and clock/packet delivery. These tests do not qualify a native codec or QUIC.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::*;
use fr_wire::*;

const STRIDE: u32 = 1_077;
const FRAGMENTS: u32 = 7;
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
fn progress(frame: u64, bytes: usize, now: u64) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame,
            total_bytes: u32::try_from(bytes).unwrap(),
            stride: STRIDE,
            capture_micros: now,
            reference: frame.checked_sub(1),
        },
        observed_micros: now,
        observation: SourceObservation::Captured,
        pipeline: PipelineState::Running,
    }
}
fn original(sender: &mut SendCache, now: u64) -> (PacketOffer, Vec<u8>) {
    let mut out = [0; 1_150];
    let offer = sender.next_packet(now, &mut out).unwrap().unwrap();
    let packet = out[..offer.byte_len()].to_vec();
    (offer, packet)
}
fn deliver(receiver: &mut ReceivePipeline, packet: &(PacketOffer, Vec<u8>), now: u64) {
    receiver
        .receive(packet.0.channel(), &packet.1, now)
        .unwrap();
}
fn index(packet: &(PacketOffer, Vec<u8>)) -> u32 {
    let c = config();
    let record = Record::decode(
        &packet.1,
        &c.limits,
        c.bindings.for_channel(Channel::Video),
        Channel::Video,
    )
    .unwrap();
    decode_fragment(record, &c.limits).unwrap().index
}
fn repair(sender: &mut SendCache, now: u64) -> Option<(PacketOffer, Vec<u8>)> {
    let mut out = [0; 1_150];
    sender
        .next_repair_packet(now, &mut out)
        .unwrap()
        .map(|offer| {
            let packet = out[..offer.byte_len()].to_vec();
            (offer, packet)
        })
}
fn fixture(policy: SendPolicy) -> (SendCache, ReceivePipeline) {
    let c = config();
    let mut sender = SendCache::new(c.limits, c.bindings, c.epoch, policy).unwrap();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    sender
        .push(progress(0, 1, 0), vec![42], DeliveryMode::Recovery, 0)
        .unwrap();
    for _ in 0..2 {
        deliver(&mut receiver, &original(&mut sender, 0), 0);
    }
    let picture = receiver.take_decodable(0).unwrap().unwrap();
    receiver.acknowledge_decode(&picture, true, 0).unwrap();
    (sender, receiver)
}

fn flight(sender: &mut SendCache, fragments: u32) -> Vec<(PacketOffer, Vec<u8>)> {
    let n = usize::try_from(STRIDE * fragments).unwrap();
    sender
        .push(
            progress(1, n, 10_000),
            vec![7; n],
            DeliveryMode::Datagrams,
            10_000,
        )
        .unwrap();
    (0..=fragments).map(|_| original(sender, 10_000)).collect()
}
fn requested(receiver: &mut ReceivePipeline, now: u64, total: u32) -> Option<Vec<RepairRange>> {
    let c = config();
    let mut out = [0; 1_150];
    receiver.repair_offer(now, &mut out).unwrap().map(|offer| {
        assert_eq!(offer.reference_deadline_us, 260_000);
        let record = Record::decode(
            &out[..offer.bytes],
            &c.limits,
            c.bindings.for_channel(Channel::Control),
            Channel::Control,
        )
        .unwrap();
        decode_repair(record, total, &c.limits)
            .unwrap()
            .ranges()
            .collect()
    })
}

#[test]
fn a_progressing_tail_does_not_spend_repair_attempts_before_a_real_tail_loss() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    // Six sequential arrivals, each sooner than the existing reorder delay.
    // The seventh/final fragment is lost. No fabricated end-of-flight signal.
    for (i, packet) in packets.iter().skip(1).take(6).enumerate() {
        let now = 10_000 + u64::try_from(i).unwrap() * 15_000;
        deliver(&mut receiver, packet, now);
        assert!(requested(&mut receiver, now, FRAGMENTS).is_none());
        assert_eq!(receiver.next_deadline(), Some(now + 20_000));
        assert!(requested(&mut receiver, now + 14_999, FRAGMENTS).is_none());
    }
    assert_eq!(receiver.next_deadline(), Some(105_000));
    assert!(requested(&mut receiver, 104_999, FRAGMENTS).is_none());
    // All three attempts remain for the actually missing final fragment.
    for now in [105_000, 165_000, 225_000] {
        assert_eq!(
            requested(&mut receiver, now, FRAGMENTS),
            Some(vec![RepairRange { start: 6, end: 7 }])
        );
    }
    assert_eq!(receiver.next_deadline(), Some(260_000));
    assert!(requested(&mut receiver, 259_999, FRAGMENTS).is_none());
    assert_eq!(receiver.tick(260_000), Err(DeliveryError::ReferenceExpired));
}

#[test]
fn confirmed_interior_holes_do_not_wait_for_the_tail_to_finish() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    for (packet, now) in [(1, 10_000), (3, 25_000), (5, 35_000)] {
        deliver(&mut receiver, &packets[packet], now);
    }
    // A later index proves holes 1 and 3; no evidence yet that 5 and 6 are lost.
    assert_eq!(
        requested(&mut receiver, 35_000, FRAGMENTS),
        Some(vec![
            RepairRange { start: 1, end: 2 },
            RepairRange { start: 3, end: 4 }
        ])
    );
    deliver(&mut receiver, &packets[2], 45_000);
    deliver(&mut receiver, &packets[4], 45_000);
    // The fixed retry cadence still applies after filling the prefix.
    assert_eq!(receiver.next_deadline(), Some(95_000));
    assert_eq!(
        requested(&mut receiver, 95_000, FRAGMENTS),
        Some(vec![RepairRange { start: 5, end: 7 }])
    );
}

#[test]
fn duplicate_fragments_and_progress_do_not_postpone_the_tail_probe() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    deliver(&mut receiver, &packets[1], 10_000);
    deliver(&mut receiver, &packets[1], 20_000);
    deliver(&mut receiver, &packets[0], 25_000);
    assert_eq!(receiver.next_deadline(), Some(30_000));
    assert_eq!(
        requested(&mut receiver, 30_000, FRAGMENTS),
        Some(vec![RepairRange { start: 1, end: 7 }])
    );
    deliver(&mut receiver, &packets[2], 80_000);
    deliver(&mut receiver, &packets[2], 95_000);
    assert_eq!(receiver.next_deadline(), Some(100_000));
    assert!(requested(&mut receiver, 99_999, FRAGMENTS).is_none());
    assert_eq!(
        requested(&mut receiver, 100_000, FRAGMENTS),
        Some(vec![RepairRange { start: 2, end: 7 }])
    );
}

#[test]
fn unique_fragment_trickle_cannot_extend_the_original_reference_deadline() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, 32);
    deliver(&mut receiver, &packets[0], 10_000);
    for (i, packet) in packets.iter().skip(1).take(31).enumerate() {
        let now = 10_000 + u64::try_from(i).unwrap() * 8_000;
        deliver(&mut receiver, packet, now);
        assert!(requested(&mut receiver, now, 32).is_none());
        assert_eq!(receiver.reference_deadline(), Some(260_000));
        assert_eq!(receiver.next_deadline(), Some((now + 20_000).min(260_000)));
    }
    assert!(requested(&mut receiver, 259_999, 32).is_none());
    assert_eq!(receiver.tick(260_000), Err(DeliveryError::ReferenceExpired));
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
}

#[test]
fn an_entirely_lost_final_picture_is_still_detected_from_its_announcement() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    let packets = flight(&mut sender, FRAGMENTS);
    deliver(&mut receiver, &packets[0], 10_000);
    assert_eq!(receiver.next_deadline(), Some(30_000));
    assert_eq!(
        requested(&mut receiver, 30_000, FRAGMENTS),
        Some(vec![RepairRange { start: 0, end: 7 }])
    );
    assert_eq!(receiver.next_deadline(), Some(90_000));
}

#[test]
fn a_late_tail_loss_can_be_repaired_on_the_same_chain_after_a_paced_flight() {
    let (mut sender, mut receiver) = fixture(SendPolicy::default());
    // Eleven paced datagrams keep making progress beyond the old three-attempt
    // schedule (30, 90 and 150 ms). Only the final twelfth datagram is lost.
    let packets = flight(&mut sender, 12);
    deliver(&mut receiver, &packets[0], 10_000);
    for (i, packet) in packets.iter().skip(1).take(11).enumerate() {
        let now = 10_000 + u64::try_from(i).unwrap() * 15_000;
        deliver(&mut receiver, packet, now);
        let mut out = [0; 1_150];
        // Drive the real schedule while delivery proceeds, not just at the end.
        receiver.repair_offer(now, &mut out).unwrap();
    }
    let mut out = [0; 1_150];
    let query = receiver
        .repair_offer(180_000, &mut out)
        .unwrap()
        .expect("repair credit must remain after the original tail");
    sender.queue_repair(&out[..query.bytes], 200_000).unwrap();
    let packet = repair(&mut sender, 200_000).unwrap();
    assert_eq!(index(&packet), 11);
    assert!(repair(&mut sender, 200_000).is_none());
    deliver(&mut receiver, &packet, 220_000);
    let picture = receiver.take_decodable(220_000).unwrap().unwrap();
    assert_eq!(
        picture.bytes(),
        vec![7; usize::try_from(12 * STRIDE).unwrap()]
    );
    assert_eq!(picture.epoch(), config().epoch);
    assert_eq!(picture.reference_deadline_us(), 260_000);
    receiver
        .acknowledge_decode(&picture, true, 220_000)
        .unwrap();
    assert!(!sender.needs_recovery());
}

#[path = "progressive_repair/prefix.rs"]
mod prefix;

#[path = "progressive_repair/reordering.rs"]
mod reordering;
