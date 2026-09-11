//! Deadline and identity guarantees for retained selective-repair records.
//! Payloads exercise real framing and reassembly, not HEVC decoding.
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
fn repair_offer_preserves_original_reference_deadline_and_legacy_wire_bytes() {
    let c = config();
    let (mut receiver, _) = running(c);
    let (mut legacy, _) = running(c);
    let d = descriptor(1, Some(0));
    let p = progress(d, c);
    receiver.receive(Channel::MediaConfig, &p, 1_000).unwrap();
    legacy.receive(Channel::MediaConfig, &p, 1_000).unwrap();
    assert_eq!(receiver.reference_deadline(), Some(251_000));
    assert_eq!(receiver.next_deadline(), Some(21_000));
    let mut out = [0; 1_150];
    let mut previous = [0; 1_150];
    let offer = receiver.repair_offer(21_000, &mut out).unwrap().unwrap();
    let n = legacy
        .repair_request(21_000, &mut previous)
        .unwrap()
        .unwrap();
    assert_eq!(
        offer,
        RepairOffer {
            bytes: n,
            frame: 1,
            reference_deadline_us: 251_000
        }
    );
    assert_eq!(out[..n], previous[..n]);
    assert!(receiver.repair_needed(1));
    assert!(!receiver.repair_needed(0));
    assert_eq!(receiver.reference_deadline(), Some(251_000));
    assert_eq!(receiver.next_deadline(), Some(81_000));
    receiver.receive(Channel::MediaConfig, &p, 75_000).unwrap();
    let retry = receiver.repair_offer(81_000, &mut out).unwrap().unwrap();
    assert_eq!(retry.reference_deadline_us, offer.reference_deadline_us);
    assert_eq!(receiver.tick(251_000), Err(DeliveryError::ReferenceExpired));
    assert!(!receiver.repair_needed(1));
}

#[test]
fn completed_picture_retires_unsent_repair_without_retiming_decode() {
    let c = config();
    let (mut receiver, _) = running(c);
    let d = descriptor(1, Some(0));
    receiver
        .receive(Channel::MediaConfig, &progress(d, c), 1_000)
        .unwrap();
    let mut out = [0; 1_150];
    let offered = receiver.repair_offer(21_000, &mut out).unwrap().unwrap();
    for packet in fragments(d, &payload(), c) {
        receiver.receive(Channel::Video, &packet, 25_000).unwrap();
    }
    assert!(!receiver.repair_needed(offered.frame));
    assert_eq!(
        receiver.reference_deadline(),
        Some(offered.reference_deadline_us)
    );
    let unit = receiver.take_decodable(30_000).unwrap().unwrap();
    assert!(!receiver.repair_needed(offered.frame));
    assert_eq!(
        receiver.reference_deadline(),
        Some(offered.reference_deadline_us)
    );
    receiver.acknowledge_decode(&unit, true, 31_000).unwrap();
    assert_eq!(receiver.reference_deadline(), None);
}

#[test]
fn undersized_repair_storage_does_not_consume_an_attempt() {
    let c = config();
    let (mut receiver, _) = running(c);
    receiver
        .receive(
            Channel::MediaConfig,
            &progress(descriptor(1, Some(0)), c),
            1_000,
        )
        .unwrap();
    assert!(receiver.repair_offer(21_000, &mut [0; 1]).is_err());
    assert_eq!(receiver.next_deadline(), Some(21_000));
    let mut out = [0; 1_150];
    assert!(receiver.repair_offer(21_000, &mut out).unwrap().is_some());
    assert_eq!(receiver.next_deadline(), Some(81_000));
}

#[test]
fn receiver_identity_check_rejects_equal_numeric_replacements_without_mutating_them() {
    use fr_core::ids::HostBootId;
    use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample, Error, ViewTracker};
    let c = config();
    let make = || {
        let mut receiver =
            ReceivePipeline::new(c, MediaBudget::new(c.limits.protocol()).unwrap()).unwrap();
        receiver.decoder_configured(0).unwrap();
        receiver
    };
    let mut receiver = make();
    let foreign = make();
    let clock = ClockCorrelation::new(
        ClockSample {
            host_boot: HostBootId::from_raw(1),
            client_sent_us: 0,
            host_sample_us: 0,
            client_received_us: 0,
        },
        ClockPolicy::default(),
    )
    .unwrap();
    let view = ViewTracker::new(&receiver, clock, 250_000, 0).unwrap();
    assert_eq!(view.check_receiver(&receiver), Ok(()));
    assert_eq!(view.check_receiver(&foreign), Err(Error::StaleBinding));
    assert_eq!(foreign.state(), ReceiveState::AwaitingRecovery);
    receiver
        .replace(
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::from_raw(1),
            },
            MediaBindings::new(5, 6, 7, 8).unwrap(),
            11_000,
        )
        .unwrap();
    assert_eq!(view.check_receiver(&receiver), Err(Error::StaleBinding));
}
