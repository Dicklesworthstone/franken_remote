//! Exact canonical pending-repair owner, real receiver and wire codecs.
//! Compressed bytes and clock advances are fixtures, not codec/native proof.
use super::*;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::delivery::{
    MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePolicy, ReceiveState,
    RecoveryRequestor,
};
use fr_wire::*;

fn config() -> ReceiveConfig {
    ReceiveConfig {
        limits: MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap(),
        bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
        epoch: MediaEpoch {
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
        policy: ReceivePolicy::default(),
    }
}
fn running(config: ReceiveConfig, stamp: u64) -> ReceivePipeline {
    let mut receiver =
        ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap()).unwrap();
    receiver.decoder_configured(stamp).unwrap();
    let mut bytes = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: stamp,
            bytes: b"data",
        },
        config.bindings.for_channel(Channel::Recovery),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(Channel::Recovery, &bytes[..n], stamp)
        .unwrap();
    let picture = receiver.take_decodable(stamp).unwrap().unwrap();
    receiver.complete_decode(&picture, stamp).unwrap();
    receiver
}
fn progress(config: ReceiveConfig, stamp: u64) -> (FrameDescriptor, Vec<u8>) {
    let descriptor = FrameDescriptor {
        frame: 1,
        reference: Some(0),
        total_bytes: 4,
        stride: 4,
        capture_micros: stamp,
    };
    let mut bytes = vec![0; 1150];
    let n = encode_progress(
        Progress {
            descriptor,
            observed_micros: stamp,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        config.bindings.for_channel(Channel::MediaConfig),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    (descriptor, bytes)
}
fn fragments(config: ReceiveConfig, descriptor: FrameDescriptor) -> Vec<u8> {
    let mut bytes = vec![0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor,
            index: 0,
            bytes: b"next",
        },
        config.bindings.for_channel(Channel::Video),
        &config.limits,
        &mut bytes,
    )
    .unwrap();
    bytes.truncate(n);
    bytes
}

fn pending() -> (ReceivePipeline, Repair, FrameDescriptor) {
    let cfg = config();
    let mut receiver = running(cfg, 0);
    let (descriptor, bytes) = progress(cfg, 0);
    receiver.receive(Channel::MediaConfig, &bytes, 0).unwrap();
    let mut repair = Repair::default();
    repair.prepare_raw(&mut receiver, 20_000).unwrap();
    assert!(repair.len > 0);
    (receiver, repair, descriptor)
}
fn binding() -> decoder::Binding {
    decoder::Binding {
        parent: negotiation::ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}

#[test]
fn exact_expiry_refuses_the_prepared_record_and_recovery_keeps_one_original_deadline() {
    let (mut receiver, mut repair, _) = pending();
    let mut recovery = RecoveryRequestor::new(&receiver, binding()).unwrap();
    let original = repair.bytes;
    let until = repair.until;
    assert!(repair.useful(&receiver, until - 1).unwrap());
    assert!(!repair.useful(&receiver, until).unwrap());
    assert_eq!(
        repair.bytes, original,
        "read-only admission check cannot reset the offer"
    );
    repair.clear();
    let mut bytes = [0; 512];
    let offer = recovery
        .offer(&mut receiver, until, &mut bytes)
        .unwrap()
        .unwrap();
    assert_eq!(receiver.state(), ReceiveState::NeedsRecovery);
    assert_eq!(receiver.budget_usage().bytes, 0);
    assert_eq!(
        offer.send_by_micros(),
        until + config().policy.recovery_budget_micros
    );
    let expected = bytes[..offer.byte_len()].to_vec();
    for now in [until + 1, until + 100_000, offer.send_by_micros() - 1] {
        assert!(!repair.useful(&receiver, now).unwrap());
        let same = recovery
            .offer(&mut receiver, now, &mut bytes)
            .unwrap()
            .unwrap();
        assert_eq!(same.send_by_micros(), offer.send_by_micros());
        assert_eq!(&bytes[..same.byte_len()], expected);
    }
    assert!(matches!(
        recovery.offer(&mut receiver, offer.send_by_micros(), &mut bytes),
        Err(DeliveryError::RecoveryExpired)
    ));
}

#[test]
fn final_fragment_arriving_after_preparation_makes_only_that_repair_obsolete() {
    let (mut receiver, mut repair, descriptor) = pending();
    receiver
        .receive(Channel::Video, &fragments(config(), descriptor), 25_000)
        .unwrap();
    assert!(!repair.useful(&receiver, 25_000).unwrap());
    repair.prepare_raw(&mut receiver, 25_000).unwrap();
    assert_eq!(repair.len, 0);
    assert!(repair.bytes.iter().all(|&b| b == 0));
    assert_eq!(receiver.state(), ReceiveState::Streaming);
    let picture = receiver.take_decodable(25_000).unwrap().unwrap();
    assert_eq!(picture.descriptor(), descriptor);
    assert_eq!(picture.bytes(), b"next");
}

#[test]
fn backpressure_checks_preserve_bytes_deadline_and_do_not_spend_another_attempt() {
    let (mut receiver, mut repair, _) = pending();
    let original = (repair.bytes, repair.len, repair.until, repair.frame);
    for now in [30_000, 50_000, 80_000, 100_000] {
        assert!(repair.useful(&receiver, now).unwrap());
        repair.prepare_raw(&mut receiver, now).unwrap();
        assert_eq!(
            (repair.bytes, repair.len, repair.until, repair.frame),
            original
        );
    }
    // The initial preparation spent one attempt. Repeated checks/prepare must
    // leave the remaining two, even though the request interval elapsed often.
    let mut bytes = [0; 548];
    assert!(
        receiver
            .repair_offer(100_000, &mut bytes)
            .unwrap()
            .is_some()
    );
    assert!(
        receiver
            .repair_offer(160_000, &mut bytes)
            .unwrap()
            .is_some()
    );
    assert!(
        receiver
            .repair_offer(220_000, &mut bytes)
            .unwrap()
            .is_none()
    );
    assert!(repair.useful(&receiver, 249_999).unwrap());
    assert_eq!(repair.until, 250_000);
}

#[test]
fn a_new_offer_after_admission_does_not_get_a_new_reference_lifetime() {
    let (mut receiver, mut repair, _) = pending();
    let first_until = repair.until;
    repair.clear();
    repair.prepare_raw(&mut receiver, 79_999).unwrap();
    assert_eq!(repair.len, 0, "original request cadence remains in force");
    repair.prepare_raw(&mut receiver, 80_000).unwrap();
    assert!(repair.len > 0);
    assert_eq!(repair.until, first_until);
    assert_eq!(repair.prepared_at, 80_000);
    assert!(!repair.useful(&receiver, first_until).unwrap());
}

#[test]
fn clock_regression_is_a_fault_not_obsolete_work_or_a_new_repair() {
    let (mut receiver, repair, _) = pending();
    assert_eq!(
        repair.useful(&receiver, 19_999),
        Err(DeliveryError::ClockRegression)
    );
    assert_eq!(receiver.tick(19_999), Err(DeliveryError::ClockRegression));
}

#[test]
fn closed_receiver_cannot_turn_a_discarded_repair_into_recovery_permission() {
    let (mut receiver, mut repair, _) = pending();
    let mut recovery = RecoveryRequestor::new(&receiver, binding()).unwrap();
    receiver.close();
    assert!(!repair.useful(&receiver, 30_000).unwrap());
    repair.clear();
    assert!(repair.prepare_raw(&mut receiver, 30_000).is_err());
    assert!(matches!(
        recovery.offer(&mut receiver, 30_000, &mut [0; 512]),
        Err(DeliveryError::StaleGeneration)
    ));
    assert_eq!(receiver.state(), ReceiveState::Closed);
}
