//! The actual verifier-to-authority transition with explicit source/clock
//! fixtures. No decoded sequence or replay floor substitutes for application.
use super::*;
use fr_core::{
    authority::{AuthorityError, AuthorityPolicy},
    ids::*,
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, Capability, InputSession},
};
use fr_wire::{
    FrameDescriptor,
    input::{InputDelivery, InputDirection},
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
fn at(micros: u64) -> HostInstant {
    HostInstant::from_micros(micros)
}
fn source(observed_us: u64) -> wire::Sample {
    wire::Sample {
        stamp: wire::Stamp {
            frame: 1,
            captured_us: 100_000,
            observed_us,
            source: SourceObservation::QualifiedUnchanged,
        },
        age_upper_us: 20_000,
    }
}
fn observe(verifier: &mut Verifier, sample: wire::Sample, now: u64) {
    verifier
        .observe(
            Progress {
                descriptor: FrameDescriptor {
                    frame: sample.stamp.frame,
                    reference: None,
                    capture_micros: sample.stamp.captured_us,
                    total_bytes: 4,
                    stride: 4,
                },
                observed_micros: sample.stamp.observed_us,
                observation: sample.stamp.source,
                pipeline: PipelineState::Idle,
            },
            now,
        )
        .unwrap();
}
fn fixture() -> (SessionAuthority, Verifier) {
    let mut authority = SessionAuthority::new(
        binding().parent.remote_session,
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(0)).unwrap();
    authority.require_view_evidence(at(0)).unwrap();
    let mut verifier = Verifier::new(binding(), ProtocolLimits::ABSOLUTE, 0).unwrap();
    observe(&mut verifier, source(120_000), 120_000);
    (authority, verifier)
}
fn apply(
    authority: &mut SessionAuthority,
    verifier: &mut Verifier,
    sequence: u64,
    visible: Option<wire::Sample>,
    now: u64,
) -> Result<bool, Error> {
    let mut bytes = [0; wire::BYTES];
    wire::encode(
        wire::Report { sequence, visible },
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    let (sequence, decision) = verifier
        .receive_report(&bytes, now, wire::BASE_SOURCE_AGE_US)
        .map_err(Error::Proof)?;
    apply_verified_report(authority, sequence, decision, at(now))
}

#[test]
fn successfully_applied_report_is_snapshotted_by_the_actual_native_ticket_owner() {
    let (mut authority, mut verifier) = fixture();
    assert!(
        apply(
            &mut authority,
            &mut verifier,
            7,
            Some(source(120_000)),
            130_000
        )
        .unwrap()
    );
    let lease = InputLeaseId::from_raw(20);
    let ticket = InputTicketId::from_raw(21);
    authority.grant_lease(lease, at(130_000)).unwrap();
    authority
        .issue_input_ticket(lease, ticket, at(130_000))
        .unwrap();
    let view = binding();
    let mut owner = InputSession::new(
        authority,
        InputCredentials {
            session: view.parent.remote_session,
            lease,
            ticket,
            view: InputView {
                geometry: view.geometry,
                viewport: view.viewport,
                configuration: view.configuration,
                recovery: view.recovery,
            },
        },
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(Capability::Keys),
        at(130_000),
    )
    .unwrap();
    let proof = owner
        .issue_readiness_ticket_timed(InputTicketId::from_raw(22), at(130_001))
        .unwrap();
    assert_eq!(proof.presented_sequence, NonZeroU64::new(7));
    assert_eq!(proof.issued_at, at(130_001));
    assert_eq!(proof.expires_at, at(1_130_001));
    assert_eq!(proof.authority_until, at(3_000_000));
}

#[test]
fn higher_obsolete_and_negative_report_floors_never_become_applied_acknowledgements() {
    let (mut authority, mut verifier) = fixture();
    assert!(
        apply(
            &mut authority,
            &mut verifier,
            1,
            Some(source(120_000)),
            130_000
        )
        .unwrap()
    );
    let lease = InputLeaseId::from_raw(20);
    authority.grant_lease(lease, at(130_000)).unwrap();
    assert!(
        !apply(
            &mut authority,
            &mut verifier,
            9,
            Some(source(120_000)),
            130_001
        )
        .unwrap()
    );
    assert_eq!(authority.presented_sequence(), NonZeroU64::new(1));
    assert!(!apply(&mut authority, &mut verifier, 10, None, 140_000).unwrap());
    assert_eq!(authority.presented_sequence(), None);
    // This later wire report is valid but cannot extend the original source.
    assert!(
        !apply(
            &mut authority,
            &mut verifier,
            11,
            Some(source(120_000)),
            150_000
        )
        .unwrap()
    );
    assert_eq!(authority.presented_sequence(), None);
    assert_eq!(
        authority.issue_presented_input_ticket(lease, InputTicketId::from_raw(22), at(150_001)),
        Err(AuthorityError::ViewUnready)
    );
}

#[test]
fn release_pending_ready_report_has_no_ack_and_later_applied_source_keeps_its_own_sequence() {
    let (mut authority, mut verifier) = fixture();
    assert!(
        apply(
            &mut authority,
            &mut verifier,
            1,
            Some(source(120_000)),
            130_000
        )
        .unwrap()
    );
    let lease = InputLeaseId::from_raw(20);
    authority.grant_lease(lease, at(130_000)).unwrap();
    assert!(!apply(&mut authority, &mut verifier, 2, None, 140_000).unwrap());
    observe(&mut verifier, source(200_000), 200_000);
    assert!(
        !apply(
            &mut authority,
            &mut verifier,
            3,
            Some(source(200_000)),
            220_000
        )
        .unwrap()
    );
    assert_eq!(authority.presented_sequence(), None);
    assert_eq!(
        authority.issue_presented_input_ticket(lease, InputTicketId::from_raw(22), at(220_000)),
        Err(AuthorityError::ViewUnready)
    );
    authority.confirm_suspension_release(lease).unwrap();
    observe(&mut verifier, source(240_000), 240_000);
    assert!(
        apply(
            &mut authority,
            &mut verifier,
            4,
            Some(source(240_000)),
            260_000
        )
        .unwrap()
    );
    assert_eq!(authority.presented_sequence(), NonZeroU64::new(4));
    authority
        .issue_presented_input_ticket(lease, InputTicketId::from_raw(22), at(260_000))
        .unwrap();
    // The source's ORIGINAL deadline still expires independently of that ticket.
    assert_eq!(
        authority.issue_presented_input_ticket(lease, InputTicketId::from_raw(23), at(490_000)),
        Err(AuthorityError::ViewUnready)
    );
    assert_eq!(authority.presented_sequence(), None);
}
