//! Production sender, receiver and codecs under a deliberately small repair-byte
//! window. Explicit packet loss/timestamps are not live QUIC or HEVC evidence.
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::*;
use fr_wire::*;

const STRIDE: usize = 1_077;
const PACKET: usize = 1_150;

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, PACKET, 16_384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}

fn sender() -> SendCache {
    let c = config();
    SendCache::new(
        c.limits,
        c.bindings,
        c.epoch,
        SendPolicy {
            repair_bytes_per_window: PACKET + 100,
            repair_window_micros: 100_000,
            max_repair_rounds: 1,
            ..SendPolicy::default()
        },
    )
    .unwrap()
}

fn push(sender: &mut SendCache, frame: u64, length: usize, at: u64) -> Vec<u8> {
    let data = vec![u8::try_from(frame).unwrap(); length];
    sender
        .push(
            Progress {
                descriptor: FrameDescriptor {
                    frame,
                    total_bytes: u32::try_from(length).unwrap(),
                    stride: u32::try_from(STRIDE).unwrap(),
                    capture_micros: at,
                    reference: frame.checked_sub(1),
                },
                observed_micros: at,
                observation: SourceObservation::Captured,
                pipeline: PipelineState::Running,
            },
            data.clone(),
            if frame == 0 {
                DeliveryMode::Recovery
            } else {
                DeliveryMode::Datagrams
            },
            at,
        )
        .unwrap();
    data
}

fn originals(sender: &mut SendCache, at: u64) -> Vec<(PacketOffer, Vec<u8>)> {
    let mut out = [0; PACKET];
    let mut packets = Vec::new();
    while let Some(offer) = sender.next_packet(at, &mut out).unwrap() {
        sender.authorize_write(&offer, at).unwrap();
        let bytes = out[..offer.byte_len()].to_vec();
        packets.push((offer, bytes));
    }
    packets
}

fn request(sender: &mut SendCache, frame: u64, fragments: u32, at: u64) {
    let c = config();
    let mut out = [0; PACKET];
    let n = encode_repair(
        frame,
        &[RepairRange {
            start: 0,
            end: fragments,
        }],
        fragments,
        c.bindings.for_channel(Channel::Control),
        &c.limits,
        &mut out,
    )
    .unwrap();
    sender.queue_repair(&out[..n], at).unwrap();
}

fn repair(sender: &mut SendCache, at: u64) -> Option<(PacketOffer, Vec<u8>)> {
    let mut out = [0; PACKET];
    sender
        .next_repair_packet(at, &mut out)
        .unwrap()
        .map(|offer| {
            sender.authorize_write(&offer, at).unwrap();
            let data = out[..offer.byte_len()].to_vec();
            (offer, data)
        })
}

fn identify(packet: &(PacketOffer, Vec<u8>)) -> (u64, u32) {
    let c = config();
    let record = Record::decode(&packet.1, &c.limits, 1, Channel::Video).unwrap();
    let fragment = decode_fragment(record, &c.limits).unwrap();
    (fragment.descriptor.frame, fragment.index)
}

fn fixture() -> SendCache {
    let mut sender = sender();
    push(&mut sender, 0, 1, 0);
    originals(&mut sender, 0);
    push(&mut sender, 1, 2 * STRIDE, 1_000);
    originals(&mut sender, 1_000);
    push(&mut sender, 2, 27, 2_000); // Exactly 100 encoded bytes.
    originals(&mut sender, 2_000);
    request(&mut sender, 1, 2, 30_000);
    request(&mut sender, 2, 1, 30_000);
    sender
}

#[test]
fn a_small_repair_uses_remaining_credit_without_displacing_an_older_job() {
    let mut sender = fixture();
    let usage = (sender.cached_bytes(), sender.cached_pictures());
    let first = repair(&mut sender, 30_000).unwrap();
    assert_eq!(identify(&first), (1, 0));
    assert_eq!(first.0.byte_len(), PACKET);
    assert_eq!(sender.next_deadline(), Some(100_000));
    let short =
        repair(&mut sender, 30_000).expect("a blocked full packet cannot hold a fitting tail");
    assert_eq!(identify(&short), (2, 0));
    assert_eq!(short.0.byte_len(), 100);
    assert_eq!(short.0.send_by_micros(), 252_000);
    assert!(repair(&mut sender, 99_999).is_none());
    assert_eq!(sender.next_deadline(), Some(100_000));
    let last = repair(&mut sender, 100_000).unwrap();
    assert_eq!(identify(&last), (1, 1));
    assert_eq!(last.0.send_by_micros(), 251_000);
    assert!(repair(&mut sender, 100_000).is_none());
    assert_eq!(sender.next_deadline(), Some(251_000));
    assert_eq!((sender.cached_bytes(), sender.cached_pictures()), usage);
    assert!(!sender.originals_pending());
    assert!(!sender.needs_recovery());
}

#[test]
fn an_output_refusal_spends_neither_credit_nor_the_fitting_jobs_cursor() {
    let mut sender = fixture();
    assert_eq!(identify(&repair(&mut sender, 30_000).unwrap()), (1, 0));
    assert!(matches!(
        sender.next_repair_packet(30_000, &mut [0; 99]),
        Err(SendError::Wire(_))
    ));
    let short = repair(&mut sender, 30_000).unwrap();
    assert_eq!(identify(&short), (2, 0));
    assert_eq!(short.0.byte_len(), 100);
    assert!(repair(&mut sender, 30_000).is_none());
    assert_eq!(identify(&repair(&mut sender, 100_000).unwrap()), (1, 1));
}

#[test]
fn fitting_repairs_keep_earliest_deadline_priority_and_exact_window_accounting() {
    let mut sender = sender();
    for (frame, size, at) in [
        (0, 1, 0),
        (1, 2 * STRIDE, 1_000),
        (2, 20, 2_000),
        (3, 10, 3_000),
    ] {
        push(&mut sender, frame, size, at);
        originals(&mut sender, at);
    }
    // Request arrival order is NOT the scheduler's priority order.
    request(&mut sender, 3, 1, 30_000);
    request(&mut sender, 2, 1, 30_000);
    request(&mut sender, 1, 2, 30_000);
    let mut first_window = 0;
    for expected in [(1, 0), (2, 0)] {
        let p = repair(&mut sender, 30_000).unwrap();
        assert_eq!(identify(&p), expected);
        first_window += p.0.byte_len();
    }
    assert_eq!(first_window, 1_243);
    assert!(repair(&mut sender, 99_999).is_none());
    let mut next_window = 0;
    for expected in [(1, 1), (3, 0)] {
        let p = repair(&mut sender, 100_000).unwrap();
        assert_eq!(identify(&p), expected);
        next_window += p.0.byte_len();
    }
    assert_eq!(next_window, 1_233);
    assert!(repair(&mut sender, 100_000).is_none());
    assert!(!sender.needs_recovery());
}

#[test]
fn spare_credit_never_emits_an_unissued_original_as_a_repair() {
    let mut sender = sender();
    push(&mut sender, 0, 1, 0);
    originals(&mut sender, 0);
    push(&mut sender, 1, 2 * STRIDE, 1_000);
    originals(&mut sender, 1_000);
    push(&mut sender, 2, 27, 2_000);
    let mut out = [0; PACKET];
    let announcement = sender.next_packet(2_000, &mut out).unwrap().unwrap();
    assert_eq!(announcement.channel(), Channel::MediaConfig);
    request(&mut sender, 1, 2, 30_000);
    request(&mut sender, 2, 1, 30_000);
    assert_eq!(identify(&repair(&mut sender, 30_000).unwrap()), (1, 0));
    for at in [30_000, 39_999] {
        assert!(repair(&mut sender, at).is_none());
    }
    let original = sender.next_packet(40_000, &mut out).unwrap().unwrap();
    assert_eq!(
        identify(&(original.clone(), out[..original.byte_len()].to_vec())),
        (2, 0)
    );
    sender.authorize_write(&original, 40_000).unwrap();
    assert_eq!(identify(&repair(&mut sender, 40_000).unwrap()), (2, 0));
    assert_eq!(identify(&repair(&mut sender, 100_000).unwrap()), (1, 1));
}

#[test]
fn two_lost_pictures_reassemble_without_an_extra_request_after_byte_refill() {
    let c = config();
    let mut sender = sender();
    let mut receiver =
        ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(0).unwrap();
    push(&mut sender, 0, 1, 0);
    for (offer, bytes) in originals(&mut sender, 0) {
        receiver.receive(offer.channel(), &bytes, 0).unwrap();
    }
    let start = receiver.take_decodable(0).unwrap().unwrap();
    receiver.complete_decode(&start, 0).unwrap();
    drop(start);
    let mut expected = Vec::new();
    for (frame, size, at) in [(1, 2 * STRIDE, 1_000), (2, 27, 2_000)] {
        expected.push(push(&mut sender, frame, size, at));
        for (offer, bytes) in originals(&mut sender, at) {
            // Entire video datagram flights are lost; reliable announcements arrive.
            if offer.channel() == Channel::MediaConfig {
                receiver.receive(offer.channel(), &bytes, at).unwrap();
            }
        }
    }
    let mut out = [0; PACKET];
    for frame in [1, 2] {
        let query = receiver.repair_offer(30_000, &mut out).unwrap().unwrap();
        assert_eq!(query.frame, frame);
        sender.queue_repair(&out[..query.bytes], 30_000).unwrap();
    }
    for (at, expected_packet) in [(30_000, (1, 0)), (30_000, (2, 0)), (100_000, (1, 1))] {
        let packet = repair(&mut sender, at).unwrap();
        assert_eq!(identify(&packet), expected_packet);
        receiver.receive(packet.0.channel(), &packet.1, at).unwrap();
    }
    for (index, data) in expected.iter().enumerate() {
        let picture = receiver.take_decodable(100_000).unwrap().unwrap();
        assert_eq!(
            picture.descriptor().frame,
            u64::try_from(index).unwrap() + 1
        );
        assert_eq!(picture.bytes(), data);
        assert_eq!(picture.epoch(), c.epoch);
        receiver.complete_decode(&picture, 100_000).unwrap();
    }
    assert!(receiver.repair_offer(100_000, &mut out).unwrap().is_none());
    assert_eq!(receiver.budget_usage(), BudgetUsage::default());
    assert!(!sender.needs_recovery());
}
