//! Obsolete positive metadata is disposable only before transport admission.
//! These source/clock fixtures do not establish native visibility or authority.
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::presented::{Error, REPORT_INTERVAL_US, Reporter};
use fr_wire::{
    SourceObservation,
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
fn pending(r: &mut Reporter, now: u64) -> (Report, u64) {
    let (bytes, until) = r.pending(now).unwrap().unwrap();
    (
        wire::decode(
            bytes,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap(),
        until,
    )
}

#[test]
fn expired_positive_is_not_reported_or_retimed_and_new_source_keeps_the_sequence() {
    let mut r = reporter();
    let old = sample(120_000);
    r.prepare(Some(old), 130_000, BOUND).unwrap();
    let (original, until) = pending(&mut r, 130_000);
    assert_eq!(until, 360_000);
    assert!(r.pending_is_positive());
    // The strict accessor still refuses late bytes. Discard is explicit and
    // cannot be confused with transport admission or successful reporting.
    assert_eq!(r.pending(until), Err(Error::Expired));
    assert!(r.discard_expired_positive(until).unwrap());
    assert!(!r.has_reported());
    assert!(!r.pending_is_positive());
    assert!(r.pending(until).unwrap().is_none());
    // Neither a later timestamp nor a larger allowed bound revives this stamp.
    r.prepare(Some(old), until + 1, wire::MAX_SOURCE_AGE_US)
        .unwrap();
    assert!(r.pending(until + 1).unwrap().is_none());
    assert!(!r.discard_expired_positive(until + 1).unwrap());
    let fresh = sample(370_000);
    r.prepare(Some(fresh), 390_000, BOUND).unwrap();
    let (next, next_until) = pending(&mut r, 390_000);
    assert_eq!(next.sequence, original.sequence);
    assert_eq!(next.visible, Some(fresh));
    assert_eq!(next_until, 620_000);
    r.queued(390_000).unwrap();
    assert!(r.has_reported());
}

#[test]
fn live_pending_metadata_keeps_exact_bytes_deadline_and_accounting() {
    let mut r = reporter();
    let value = sample(120_000);
    r.prepare(Some(value), 130_000, BOUND).unwrap();
    let (bytes, until) = r.pending(130_000).unwrap().unwrap();
    let bytes = bytes.to_vec();
    for now in [130_000, 150_000, until - 1] {
        assert!(!r.discard_expired_positive(now).unwrap());
        r.prepare(Some(value), now, wire::MAX_SOURCE_AGE_US).unwrap();
        assert_eq!(r.pending(now).unwrap(), Some((bytes.as_slice(), until)));
        assert!(!r.has_reported());
    }
    assert!(r.discard_expired_positive(until).unwrap());
}

#[test]
fn discarding_positive_preserves_the_required_loss_report_and_its_deadline() {
    let mut r = reporter();
    r.prepare(Some(sample(120_000)), 130_000, BOUND).unwrap();
    r.queued(130_000).unwrap();
    let late = sample(200_000);
    r.prepare(Some(late), 220_000, BOUND).unwrap();
    let until = pending(&mut r, 220_000).1;
    assert!(r.discard_expired_positive(until).unwrap());
    // Treating this expired source as unavailable must still notify a host
    // which previously received a positive report; nothing was rolled back.
    r.prepare(Some(late), until, BOUND).unwrap();
    let (loss, loss_until) = pending(&mut r, until);
    assert_eq!(
        loss,
        Report {
            sequence: 2,
            visible: None,
        }
    );
    assert_eq!(loss_until, until + REPORT_INTERVAL_US);
    assert!(!r.pending_is_positive());
    r.prepare(Some(sample(until + 1)), until + 2, BOUND).unwrap();
    assert_eq!(pending(&mut r, until + 2), (loss, loss_until));
    assert_eq!(r.discard_expired_positive(loss_until), Err(Error::Expired));
    assert_eq!(r.prepare(None, loss_until, BOUND), Err(Error::Expired));
    assert_eq!(r.pending(loss_until), Err(Error::Expired));
    assert!(r.has_reported(), "earlier admission must not be erased");
}

#[test]
fn discarding_cannot_retract_admitted_evidence_or_hide_clock_regression() {
    let mut r = reporter();
    let value = sample(120_000);
    r.prepare(Some(value), 130_000, BOUND).unwrap();
    let original = pending(&mut r, 130_000);
    assert_eq!(r.discard_expired_positive(129_999), Err(Error::Clock));
    assert_eq!(pending(&mut r, 130_000), original);
    r.queued(130_000).unwrap();
    assert!(!r.discard_expired_positive(500_000).unwrap());
    assert!(r.has_reported());
    r.prepare(Some(value), 500_000, BOUND).unwrap();
    assert!(r.pending(500_000).unwrap().is_none());
    r.prepare(Some(sample(510_000)), 530_000, BOUND).unwrap();
    assert_eq!(pending(&mut r, 530_000).0.sequence, 2);
}

#[test]
fn repeated_expiry_does_not_accumulate_records_or_fabricate_admission() {
    let mut r = reporter();
    for index in 0..32 {
        let at = 200_000 + index * BOUND;
        let value = Sample {
            age_upper_us: BOUND - 1,
            ..sample(at)
        };
        r.prepare(Some(value), at, BOUND).unwrap();
        assert_eq!(pending(&mut r, at).0.sequence, 1);
        assert!(r.discard_expired_positive(at + 1).unwrap());
        assert!(r.pending(at + 1).unwrap().is_none());
        assert!(!r.has_reported());
    }
}
