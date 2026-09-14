use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::presented::*;
use fr_wire::{
    FrameDescriptor, PipelineState, Progress, SourceObservation,
    decoder::Binding,
    input::{InputDelivery as D, InputDirection as I},
    negotiation::ControlBinding,
    presented::{self as wire, Report, Sample, Stamp},
};
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
fn sample(observed: u64) -> Sample {
    Sample {
        stamp: Stamp {
            frame: 0,
            captured_us: 100_000,
            observed_us: observed,
            source: SourceObservation::QualifiedUnchanged,
        },
        age_upper_us: 20_000,
    }
}
fn progress(s: Sample) -> Progress {
    Progress {
        descriptor: FrameDescriptor {
            frame: s.stamp.frame,
            reference: None,
            capture_micros: s.stamp.captured_us,
            total_bytes: 4,
            stride: 4,
        },
        observed_micros: s.stamp.observed_us,
        observation: s.stamp.source,
        pipeline: PipelineState::Idle,
    }
}
fn record(sequence: u64, visible: Option<Sample>) -> Vec<u8> {
    let mut b = vec![0; wire::BYTES];
    wire::encode(
        Report { sequence, visible },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut b,
        I::ViewerToHost,
        D::Reliable,
    )
    .unwrap();
    b
}
fn verifier() -> Verifier {
    Verifier::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap()
}
#[test]
fn delayed_report_keeps_the_original_host_source_expiry() {
    let mut v = verifier();
    let s = sample(120_000);
    v.observe(progress(s), 120_000).unwrap();
    assert_eq!(
        v.receive(&record(1, Some(s)), 300_000).unwrap(),
        Decision::Ready { until_us: 370_000 }
    );
    assert_eq!(
        v.receive(&record(2, Some(s)), 369_999).unwrap(),
        Decision::Obsolete
    );
    assert_eq!(
        v.receive(&record(3, Some(s)), 370_000).unwrap(),
        Decision::Obsolete
    );
}
#[test]
fn never_issued_and_mismatched_frame_source_are_not_evidence() {
    let mut v = verifier();
    let s = sample(120_000);
    v.observe(progress(s), 120_000).unwrap();
    let mut forged = s;
    forged.stamp.frame = 1;
    assert_eq!(
        v.receive(&record(1, Some(forged)), 130_000),
        Err(Error::UnknownSource)
    );
    forged = s;
    forged.stamp.observed_us += 1;
    assert_eq!(
        v.receive(&record(2, Some(forged)), 130_000),
        Err(Error::UnknownSource)
    );
}
#[test]
fn aged_report_and_unavailable_never_establish_readiness() {
    let mut v = verifier();
    let s = sample(120_000);
    v.observe(progress(s), 120_000).unwrap();
    assert_eq!(
        v.receive(&record(1, Some(s)), 370_000).unwrap(),
        Decision::Unavailable
    );
    assert_eq!(
        v.receive(&record(2, None), 370_001).unwrap(),
        Decision::Unavailable
    );
}
#[test]
fn repeated_reports_and_regressed_clock_are_rejected() {
    let mut v = verifier();
    let s = sample(120_000);
    v.observe(progress(s), 120_000).unwrap();
    let b = record(1, Some(s));
    v.receive(&b, 130_000).unwrap();
    assert_eq!(v.receive(&b, 130_001), Err(Error::Replay));
    assert_eq!(v.receive(&record(2, Some(s)), 130_000), Err(Error::Clock));
}
#[test]
fn proof_history_is_fixed_and_eviction_cannot_authorize_unknown_source() {
    let mut v = verifier();
    for n in 0..=HISTORY {
        let s = sample(120_000 + u64::try_from(n).unwrap());
        v.observe(progress(s), 130_000).unwrap();
    }
    assert_eq!(
        v.receive(&record(1, Some(sample(120_000))), 130_000),
        Err(Error::UnknownSource)
    );
    let s = sample(120_000 + u64::try_from(HISTORY).unwrap());
    assert!(matches!(
        v.receive(&record(2, Some(s)), 130_000),
        Ok(Decision::Ready { .. })
    ));
}
#[test]
fn unknown_host_source_retires_previous_provenance() {
    let mut v = verifier();
    let s = sample(120_000);
    let mut p = progress(s);
    v.observe(p, 120_000).unwrap();
    p.observation = SourceObservation::Unknown;
    p.observed_micros = 0;
    v.observe(p, 125_000).unwrap();
    assert_eq!(
        v.receive(&record(1, Some(s)), 130_000),
        Err(Error::UnknownSource)
    );
}
#[test]
fn pending_report_preserves_bytes_and_deadline_under_backpressure() {
    let mut r = Reporter::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    let s = sample(120_000);
    r.prepare(Some(s), 130_000).unwrap();
    let (b, until) = r.pending(130_000).unwrap().unwrap();
    let b = b.to_vec();
    assert_eq!(until, 360_000);
    r.prepare(
        Some(Sample {
            age_upper_us: 100_000,
            ..s
        }),
        210_000,
    )
    .unwrap();
    assert_eq!(r.pending(210_000).unwrap(), Some((b.as_slice(), until)));
    assert_eq!(r.pending(360_000), Err(Error::Expired));
    assert!(!r.has_reported());
}
#[test]
fn same_source_does_not_emit_heartbeats_and_visibility_loss_discards_unsent() {
    let mut r = Reporter::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    let s = sample(120_000);
    r.prepare(Some(s), 130_000).unwrap();
    r.queued(130_001).unwrap();
    assert!(r.has_reported());
    r.prepare(Some(s), 200_000).unwrap();
    assert!(r.pending(200_000).unwrap().is_none());
    r.prepare(Some(sample(200_000)), 220_000).unwrap();
    assert!(r.pending(220_000).unwrap().is_some());
    r.prepare(None, 220_001).unwrap();
    let (negative, until) = r.pending(220_001).unwrap().unwrap();
    assert_eq!(negative, record(2, None));
    assert_eq!(until, 220_001 + REPORT_INTERVAL_US);
    r.queued(220_002).unwrap();
    r.prepare(None, 220_003).unwrap();
    assert!(r.pending(220_003).unwrap().is_none());
    r.prepare(Some(s), 300_000).unwrap();
    assert!(r.pending(300_000).unwrap().is_none());
}
#[test]
fn new_source_replaces_only_unsent_metadata_and_roundtrips_through_verifier() {
    let mut r = Reporter::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    let mut v = verifier();
    let old = sample(120_000);
    r.prepare(Some(old), 130_000).unwrap();
    let new = sample(140_000);
    v.observe(progress(new), 140_000).unwrap();
    r.prepare(Some(new), 150_000).unwrap();
    let (b, _) = r.pending(150_000).unwrap().unwrap();
    assert_eq!(
        v.receive(b, 180_000).unwrap(),
        Decision::Ready { until_us: 390_000 }
    );
    r.queued(150_001).unwrap();
}

#[test]
fn negative_report_preempts_positive_throttle_and_survives_later_visibility() {
    let mut r = Reporter::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    let old = sample(120_000);
    r.prepare(Some(old), 130_000).unwrap();
    r.queued(130_000).unwrap();
    r.prepare(None, 130_001).unwrap();
    let (b, until) = r.pending(130_001).unwrap().unwrap();
    let original = b.to_vec();
    assert_eq!(original, record(2, None));
    assert_eq!(until, 180_001);
    r.prepare(Some(sample(140_000)), 150_000).unwrap();
    assert_eq!(
        r.pending(150_000).unwrap(),
        Some((original.as_slice(), until))
    );
    r.queued(150_001).unwrap();
    r.prepare(Some(sample(200_000)), 220_000).unwrap();
    assert_eq!(
        r.pending(220_000).unwrap().unwrap().0,
        record(3, Some(sample(200_000)))
    );
}
#[test]
fn never_visible_emits_nothing_and_negative_expiry_cannot_be_retimed() {
    let mut r = Reporter::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    r.prepare(None, 100_000).unwrap();
    assert!(r.pending(100_000).unwrap().is_none());
    r.prepare(Some(sample(120_000)), 130_000).unwrap();
    r.prepare(None, 130_001).unwrap();
    assert!(r.pending(130_001).unwrap().is_none());
    r.prepare(Some(sample(140_000)), 150_000).unwrap();
    r.queued(150_000).unwrap();
    r.prepare(None, 150_001).unwrap();
    assert_eq!(r.prepare(None, 200_001), Err(Error::Expired));
    assert_eq!(
        r.prepare(Some(sample(180_000)), 200_002),
        Err(Error::Expired)
    );
    assert_eq!(r.pending(200_003), Err(Error::Expired));
}

#[test]
fn unconfirmed_candidate_pauses_positive_reports_without_erasing_explicit_loss() {
    let mut r = Reporter::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    let s = sample(120_000);
    r.prepare(Some(s), 130_000).unwrap();
    r.pause(130_001).unwrap();
    assert!(r.pending(130_001).unwrap().is_none());
    assert!(!r.has_reported());
    r.prepare(Some(s), 130_002).unwrap();
    r.queued(130_002).unwrap();
    r.pause(130_003).unwrap();
    assert!(r.pending(130_003).unwrap().is_none());
    r.prepare(None, 130_004).unwrap();
    let until = r.pending(130_004).unwrap().unwrap().1;
    r.pause(140_000).unwrap();
    assert_eq!(
        r.pending(140_000).unwrap(),
        Some((record(2, None).as_slice(), until))
    );
    assert_eq!(r.pause(until), Err(Error::Expired));
}
