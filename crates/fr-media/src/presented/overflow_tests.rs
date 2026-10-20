//! Counter-edge fixtures exercise the real reporter without billions of sends.
use super::*;
use fr_core::ids::*;
use fr_wire::negotiation::ControlBinding;

fn reporter() -> Reporter {
    Reporter::new(
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
        },
        ProtocolLimits::ABSOLUTE,
        0,
    )
    .unwrap()
}

#[test]
fn sequence_exhaustion_never_partially_admits_or_reuses_the_pending_report() {
    let mut r = reporter();
    r.sequence = u64::MAX;
    r.force_unavailable(1).unwrap();
    let cursor = r.admitted_cursor();
    let original = r.pending.as_ref().unwrap().bytes;
    let until = r.pending.as_ref().unwrap().until;
    assert_eq!(r.queued_receipt(2), Err(Error::Overflow));
    assert_eq!(r.admitted_cursor(), cursor);
    assert_eq!(r.pending(2).unwrap(), Some((original.as_slice(), until)));
    assert_eq!(r.force_unavailable(3), Ok(u64::MAX));
    assert_eq!(r.queued(3), Err(Error::Overflow));
    assert_eq!(r.admitted_cursor(), cursor);
}

#[test]
fn post_admission_cadence_overflow_leaves_the_original_source_and_cursor_unchanged() {
    let mut r = reporter();
    let at = u64::MAX - 2;
    r.prepare(
        Some(Sample {
            stamp: Stamp {
                frame: 1,
                captured_us: 100,
                observed_us: 100,
                source: SourceObservation::Captured,
            },
            age_upper_us: wire::BASE_SOURCE_AGE_US - 1,
        }),
        at,
        wire::BASE_SOURCE_AGE_US,
    )
    .unwrap();
    let cursor = r.admitted_cursor();
    let original = r.pending.as_ref().unwrap().bytes;
    assert_eq!(r.queued_receipt(at), Err(Error::Overflow));
    assert_eq!(r.pending(at).unwrap(), Some((original.as_slice(), at + 1)));
    assert_eq!(r.admitted_cursor(), cursor);
    assert!(!r.has_reported());
}
