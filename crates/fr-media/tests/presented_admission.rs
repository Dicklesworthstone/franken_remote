//! Explicit source/clock and transport-admission fixtures. A queued receipt
//! proves the reporter hook ran; these tests do not qualify native visibility.
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::presented::{Decision, Error, REPORT_INTERVAL_US, Reporter, Verifier};
use fr_wire::{
    FrameDescriptor, PipelineState, Progress, SourceObservation,
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    negotiation::ControlBinding,
    presented::{self as wire, Report, Sample, Stamp},
};

const BOUND: u64 = wire::BASE_SOURCE_AGE_US;
fn binding() -> Binding {
    Binding {
        parent: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(1),
            os_session: OsSessionId::from_raw(2),
            remote_session: RemoteSessionId::from_raw(3),
        },
        display: 4,
        geometry: DisplayGeometryGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
    }
}
fn sample(observed_us: u64) -> Sample {
    Sample {
        stamp: Stamp {
            frame: 1,
            captured_us: 100_000,
            observed_us,
            source: SourceObservation::QualifiedUnchanged,
        },
        age_upper_us: 20_000,
    }
}
fn reporter() -> Reporter {
    Reporter::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap()
}
fn pending(r: &mut Reporter, now: u64) -> (Vec<u8>, u64) {
    let (bytes, until) = r.pending(now).unwrap().unwrap();
    (bytes.to_vec(), until)
}
fn decode(bytes: &[u8]) -> Report {
    wire::decode(
        bytes,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap()
}

#[test]
fn preparation_replacement_and_pause_cannot_advance_the_admitted_cursor() {
    let mut r = reporter();
    let original = r.admitted_cursor();
    assert_eq!(original.sequence(), 0);
    assert_eq!(original.visible(), None);
    r.prepare(Some(sample(120_000)), 130_000, BOUND).unwrap();
    r.prepare(Some(sample(140_000)), 150_000, BOUND).unwrap();
    assert_eq!(r.admitted_cursor(), original);
    r.pause(150_001).unwrap();
    assert_eq!(r.pending(150_001).unwrap(), None);
    assert_eq!(r.admitted_cursor(), original);

    let visible = sample(160_000);
    r.prepare(Some(visible), 180_000, BOUND).unwrap();
    let receipt = r.queued_receipt(180_001).unwrap();
    assert_eq!(receipt.binding(), binding());
    assert_eq!(receipt.sequence(), 1);
    assert_eq!(receipt.visible(), Some(visible.stamp));
    assert!(receipt.is_from(&original));
    let admitted = r.admitted_cursor();
    assert_eq!(admitted.sequence(), 1);
    assert_eq!(admitted.visible(), Some(visible.stamp));
    assert!(admitted.is_from(&original));
    assert_ne!(admitted, original, "a cursor is an immutable snapshot");
    assert_eq!(r.queued_receipt(180_002), Err(Error::Expired));
    assert_eq!(r.admitted_cursor(), admitted);
}

#[test]
fn a_forced_first_lapse_keeps_its_negative_bytes_sequence_and_original_deadline() {
    let mut r = reporter();
    let original = r.admitted_cursor();
    assert_eq!(r.force_unavailable(130_000), Ok(1));
    let (bytes, until) = pending(&mut r, 130_000);
    assert_eq!(until, 130_000 + REPORT_INTERVAL_US);
    assert_eq!(
        decode(&bytes),
        Report {
            sequence: 1,
            visible: None
        }
    );
    for at in [130_001, 140_000, until - 1] {
        assert_eq!(r.force_unavailable(at), Ok(1));
        r.prepare(Some(sample(140_000)), at, BOUND).unwrap();
        r.pause(at).unwrap();
        assert_eq!(pending(&mut r, at), (bytes.clone(), until));
        assert_eq!(r.admitted_cursor(), original);
    }
    assert_eq!(r.force_unavailable(until), Err(Error::Expired));
    assert_eq!(r.queued_receipt(until), Err(Error::Expired));
    assert_eq!(r.discard_expired_positive(until + 1), Err(Error::Expired));
    assert_eq!(r.admitted_cursor(), original);
}

#[test]
fn a_new_lapse_after_an_admitted_negative_still_requires_a_new_barrier() {
    let mut r = reporter();
    assert_eq!(r.force_unavailable(10), Ok(1));
    // Existing queued callers also advance the exact same admission cursor.
    r.queued(11).unwrap();
    let first = r.admitted_cursor();
    assert_eq!(first.sequence(), 1);
    assert_eq!(first.visible(), None);
    assert_eq!(r.force_unavailable(12), Ok(2));
    let (bytes, until) = pending(&mut r, 12);
    assert_eq!(
        decode(&bytes),
        Report {
            sequence: 2,
            visible: None
        }
    );
    assert_eq!(until, 12 + REPORT_INTERVAL_US);
    assert_eq!(r.admitted_cursor(), first);
    let second = r.queued_receipt(13).unwrap();
    assert_eq!(second.sequence(), 2);
    assert_eq!(second.visible(), None);
    assert!(second.is_from(&first));
}

#[test]
fn a_reserved_negative_precedes_fresh_positive_admission_without_changing_cadence() {
    let mut r = reporter();
    r.prepare(Some(sample(120_000)), 130_000, BOUND).unwrap();
    let first = r.queued_receipt(130_000).unwrap();
    assert_eq!(first.sequence(), 1);
    assert_eq!(r.force_unavailable(130_001), Ok(2));
    r.prepare(Some(sample(140_000)), 150_000, BOUND).unwrap();
    let negative = r.queued_receipt(150_000).unwrap();
    assert_eq!(negative.sequence(), 2);
    assert_eq!(negative.visible(), None);
    let fresh = sample(160_000);
    r.prepare(Some(fresh), 150_001, BOUND).unwrap();
    assert_eq!(r.pending(150_001).unwrap(), None);
    r.prepare(Some(fresh), 200_000, BOUND).unwrap();
    let after = r.queued_receipt(200_000).unwrap();
    assert_eq!(after.sequence(), 3);
    assert_eq!(after.visible(), Some(fresh.stamp));
}

#[test]
fn identical_numeric_bindings_and_recovery_replacements_cannot_substitute_reporters() {
    let mut original = reporter();
    let cursor = original.admitted_cursor();
    original.force_unavailable(1).unwrap();
    let receipt = original.queued_receipt(2).unwrap();
    let mut foreign = reporter();
    foreign.force_unavailable(1).unwrap();
    let forged_owner = foreign.queued_receipt(2).unwrap();
    assert_eq!(receipt.binding(), forged_owner.binding());
    assert_eq!(receipt.sequence(), forged_owner.sequence());
    assert!(!forged_owner.is_from(&cursor));
    assert!(!foreign.admitted_cursor().is_from(&cursor));
    assert_ne!(receipt, forged_owner);
    drop(original);
    // Retaining the original cursor/receipt prevents token reuse after drop.
    assert!(receipt.is_from(&cursor));
    assert!(!reporter().admitted_cursor().is_from(&cursor));
    let mut recovered = binding();
    recovered.recovery = recovered.recovery.next().unwrap();
    let mut replacement = Reporter::new(recovered, ProtocolLimits::ABSOLUTE, 0).unwrap();
    replacement.force_unavailable(3).unwrap();
    assert!(!replacement.queued_receipt(4).unwrap().is_from(&cursor));
}

#[test]
fn forcing_loss_after_positive_expiry_preserves_the_source_tombstone() {
    let mut r = reporter();
    let stale = Sample {
        age_upper_us: BOUND - 1,
        ..sample(120_000)
    };
    r.prepare(Some(stale), 130_000, BOUND).unwrap();
    let original = r.admitted_cursor();
    assert_eq!(r.queued_receipt(130_001), Err(Error::Expired));
    assert_eq!(r.force_unavailable(130_001), Ok(1));
    assert_eq!(r.admitted_cursor(), original);
    r.queued_receipt(130_002).unwrap();
    r.prepare(Some(stale), 200_000, wire::MAX_SOURCE_AGE_US)
        .unwrap();
    assert_eq!(r.pending(200_000).unwrap(), None);
    assert_eq!(r.admitted_cursor().sequence(), 1);
    r.prepare(Some(sample(210_000)), 230_000, BOUND).unwrap();
    assert_eq!(r.queued_receipt(230_000).unwrap().sequence(), 2);
}

#[test]
fn clock_regression_and_time_overflow_cannot_fabricate_admission() {
    let mut r = reporter();
    r.force_unavailable(100).unwrap();
    let original = pending(&mut r, 100);
    let cursor = r.admitted_cursor();
    assert_eq!(r.force_unavailable(99), Err(Error::Clock));
    assert_eq!(r.queued_receipt(99), Err(Error::Clock));
    assert_eq!(pending(&mut r, 100), original);
    assert_eq!(r.admitted_cursor(), cursor);
    r.queued_receipt(100).unwrap();
    let admitted = r.admitted_cursor();
    assert_eq!(r.force_unavailable(u64::MAX), Err(Error::Overflow));
    assert_eq!(r.admitted_cursor(), admitted);
}

#[test]
fn verifier_returns_the_exact_sequence_with_each_decision_not_a_readiness_claim() {
    let mut verifier = Verifier::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    let visible = sample(120_000);
    verifier
        .observe(
            Progress {
                descriptor: FrameDescriptor {
                    frame: visible.stamp.frame,
                    reference: None,
                    capture_micros: visible.stamp.captured_us,
                    total_bytes: 4,
                    stride: 4,
                },
                observed_micros: visible.stamp.observed_us,
                observation: visible.stamp.source,
                pipeline: PipelineState::Idle,
            },
            120_000,
        )
        .unwrap();
    let mut bytes = [0; wire::BYTES];
    for (sequence, visible, decision) in [
        (4, Some(visible), Decision::Ready { until_us: 370_000 }),
        (9, Some(visible), Decision::Obsolete),
        (11, None, Decision::Unavailable),
    ] {
        wire::encode(
            Report { sequence, visible },
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        let (acknowledgement, actual) = verifier.receive_report(&bytes, 130_000, BOUND).unwrap();
        assert_eq!(acknowledgement.get(), sequence);
        assert_eq!(actual, decision);
    }
    assert_eq!(
        verifier.receive_report(&bytes, 130_001, BOUND),
        Err(Error::Replay)
    );
}
