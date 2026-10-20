//! Applied presentation acknowledgements on the original serialized input owner.
use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, MAX_LIVE_INPUT_TICKETS, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{
        DesktopPoint, InputBounds, InputCredentials, InputEvent, InputRequest, InputView,
        KeyTransition, PhysicalKey,
    },
    input_sequence::InputOutcome,
    input_submission::{
        Capabilities, Capability, Dispatch, InputSession, InputSink, Operation, PlatformError,
        Refusal, Submission,
    },
    time::HostInstant,
};
use std::{
    num::NonZeroU64,
    sync::{Arc, Mutex},
};

fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn sequence(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).unwrap()
}
fn ticket(value: u128) -> InputTicketId {
    InputTicketId::from_raw(value)
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: ticket(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn setup(applied: Option<NonZeroU64>) -> (Arc<Mutex<SessionAuthority>>, InputSession) {
    let c = credentials();
    let mut authority = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(0)).unwrap();
    authority.require_view_evidence(at(0)).unwrap();
    if let Some(sequence) = applied {
        authority
            .mark_presented_ready_until(sequence, at(1_500_000), at(0))
            .unwrap();
    } else {
        authority
            .mark_view_ready_until(at(1_500_000), at(0))
            .unwrap();
    }
    authority.grant_lease(c.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(c.lease, c.ticket, at(0))
        .unwrap();
    let shared = Arc::new(Mutex::new(authority));
    let native = InputSession::from_shared_authority(
        shared.clone(),
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(Capability::Keys),
        at(0),
    )
    .unwrap();
    (shared, native)
}
#[derive(Default)]
struct Sink(Vec<Operation>);
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        self.0.push(operation);
        Submission::Submitted
    }
}

#[test]
fn absent_applied_proof_refuses_without_consuming_any_ticket_slot() {
    let (_, mut native) = setup(None);
    for id in 100..132 {
        assert_eq!(
            native.issue_readiness_ticket_timed(ticket(id), at(10)),
            Err(Refusal::Authority(AuthorityError::ViewUnready))
        );
    }
    for id in 1..MAX_LIVE_INPUT_TICKETS {
        let proof = native
            .issue_ticket_proof_timed(ticket(100 + id as u128), at(10))
            .unwrap();
        assert_eq!(proof.presented_sequence, None);
    }
    assert_eq!(
        native.issue_ticket_proof_timed(ticket(200), at(10)),
        Err(Refusal::Authority(AuthorityError::TicketCapacity))
    );
    assert!(!native.monitor().is_revoked());
}

#[test]
fn issuance_atomically_samples_applied_proof_time_and_original_authority() {
    let (shared, mut native) = setup(Some(sequence(7)));
    shared
        .lock()
        .unwrap()
        .authorize_observation_delivery(at(500))
        .unwrap();
    let proof = native
        .issue_readiness_ticket_timed(ticket(4), at(100))
        .unwrap();
    assert_eq!(proof.presented_sequence, Some(sequence(7)));
    assert_eq!(proof.issued_at, at(500));
    assert_eq!(proof.expires_at, at(1_000_500));
    assert_eq!(proof.authority_until, at(3_000_000));
    assert_eq!(native.monitor().deadline(at(500)), Ok(at(1_500_000)));
    assert_eq!(
        native.ticket_credentials(ticket(4)).view,
        credentials().view
    );
    // The original ticket's exclusive expiry is not replaced by the new proof.
    assert_eq!(
        native.monitor().authorize_ticket(ticket(3), at(1_000_000)),
        Err(Refusal::Authority(AuthorityError::TicketExpired))
    );
}

#[test]
fn elapsed_view_requires_actual_release_and_a_later_successful_report() {
    let (shared, mut native) = setup(Some(sequence(7)));
    let mut sink = Sink::default();
    let dispatched = native
        .dispatch(
            InputRequest {
                credentials: credentials(),
                sequence: 0,
                event: InputEvent::Key {
                    key: PhysicalKey::new(4).unwrap(),
                    transition: KeyTransition::Press,
                },
            },
            &mut sink,
            || at(100),
        )
        .unwrap();
    assert!(matches!(dispatched, Dispatch::Completed(receipt)
        if receipt.outcome == InputOutcome::SubmittedToOs));
    assert_eq!(sink.0.len(), 1);
    {
        let mut authority = shared.lock().unwrap();
        // A positive report arriving after source expiry cannot acknowledge
        // readiness while the native key is still held, even without a tick.
        assert_eq!(
            authority.mark_presented_ready_until(sequence(8), at(2_000_000), at(1_500_000)),
            Err(AuthorityError::ReleasePending)
        );
        assert_eq!(authority.presented_sequence(), None);
    }
    assert_eq!(
        native.issue_readiness_ticket_timed(ticket(4), at(1_500_000)),
        Err(Refusal::Authority(AuthorityError::ViewUnready))
    );
    assert_eq!(native.maintain(at(1_500_001), &mut sink).remaining, 0);
    assert_eq!(sink.0.len(), 2);
    assert!(sink.0[1].is_release());
    // Release completion itself does not retroactively apply the rejected report.
    assert_eq!(
        native.issue_readiness_ticket_timed(ticket(4), at(1_500_001)),
        Err(Refusal::Authority(AuthorityError::ViewUnready))
    );
    shared
        .lock()
        .unwrap()
        .mark_presented_ready_until(sequence(9), at(2_000_000), at(1_500_002))
        .unwrap();
    let proof = native
        .issue_readiness_ticket_timed(ticket(4), at(1_500_002))
        .unwrap();
    assert_eq!(proof.presented_sequence, Some(sequence(9)));
    assert_eq!(proof.authority_until, at(3_000_000));
    assert_eq!(native.monitor().deadline(at(1_500_002)), Ok(at(2_000_000)));
    assert_eq!(
        native.monitor().authorize_ticket(ticket(3), at(1_500_003)),
        Err(Refusal::Authority(AuthorityError::TicketInvalid))
    );
}

#[test]
fn every_lapse_clears_the_previous_successful_acknowledgement() {
    let (shared, mut native) = setup(Some(sequence(7)));
    for (step, next_sequence) in [(10, 9), (20, 11)] {
        {
            let mut authority = shared.lock().unwrap();
            authority.authorize_observation_delivery(at(step)).unwrap();
            authority.mark_view_stale();
            assert_eq!(authority.presented_sequence(), None);
        }
        assert_eq!(
            native.issue_readiness_ticket_timed(ticket(u128::from(next_sequence)), at(step)),
            Err(Refusal::Authority(AuthorityError::ViewUnready))
        );
        assert_eq!(native.maintain(at(step), &mut Sink::default()).remaining, 0);
        shared
            .lock()
            .unwrap()
            .mark_presented_ready_until(sequence(next_sequence), at(1_500_000), at(step + 1))
            .unwrap();
        let proof = native
            .issue_readiness_ticket_timed(ticket(u128::from(next_sequence)), at(step + 1))
            .unwrap();
        assert_eq!(proof.presented_sequence, Some(sequence(next_sequence)));
        assert_eq!(proof.authority_until, at(3_000_000));
    }
}

#[test]
fn legacy_readiness_setters_cannot_retain_an_authenticated_acknowledgement() {
    let (shared, mut native) = setup(Some(sequence(7)));
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(1_500_000), at(10))
        .unwrap();
    assert_eq!(
        native
            .issue_ticket_proof_timed(ticket(4), at(10))
            .unwrap()
            .presented_sequence,
        None
    );
    assert_eq!(
        native.issue_readiness_ticket_timed(ticket(5), at(10)),
        Err(Refusal::Authority(AuthorityError::ViewUnready))
    );
    let mut authority = shared.lock().unwrap();
    authority
        .mark_presented_ready_until(sequence(8), at(1_500_000), at(11))
        .unwrap();
    assert_eq!(
        authority.mark_view_ready(at(12)),
        Err(AuthorityError::ViewUnready)
    );
    assert_eq!(authority.presented_sequence(), None);
}

#[test]
fn recovery_clears_proof_and_scopes_restarted_report_sequences_to_new_view() {
    let (shared, mut native) = setup(Some(sequence(99)));
    let next = RecoveryGeneration::INITIAL.next().unwrap();
    {
        let mut authority = shared.lock().unwrap();
        authority.require_view_evidence(at(10)).unwrap();
        authority.advance_recovery(next).unwrap();
        assert_eq!(authority.presented_sequence(), None);
    }
    assert_eq!(
        native.issue_readiness_ticket_timed(ticket(4), at(10)),
        Err(Refusal::Authority(AuthorityError::ViewUnready))
    );
    assert_eq!(native.maintain(at(10), &mut Sink::default()).remaining, 0);
    shared
        .lock()
        .unwrap()
        .mark_presented_ready_until(sequence(1), at(1_500_000), at(11))
        .unwrap();
    let proof = native
        .issue_readiness_ticket_timed(ticket(4), at(11))
        .unwrap();
    assert_eq!(proof.presented_sequence, Some(sequence(1)));
    assert_eq!(native.ticket_credentials(ticket(4)).view.recovery, next);
    assert_eq!(proof.authority_until, at(3_000_000));
    assert_eq!(
        native.monitor().authorize_ticket(ticket(3), at(12)),
        Err(Refusal::Authority(AuthorityError::TicketInvalid))
    );
}

#[test]
fn terminal_clear_and_replaced_numeric_lease_never_revalidate_old_provenance() {
    for clear in [
        SessionAuthority::close,
        SessionAuthority::refuse,
        SessionAuthority::revoke_lease,
        SessionAuthority::invalidate_for_suspend,
    ] {
        let (shared, mut native) = setup(Some(sequence(7)));
        {
            let mut authority = shared.lock().unwrap();
            clear(&mut authority);
            assert_eq!(authority.presented_sequence(), None);
        }
        assert!(
            native
                .issue_readiness_ticket_timed(ticket(4), at(10))
                .is_err()
        );
    }
    let (shared, mut native) = setup(Some(sequence(7)));
    {
        let mut authority = shared.lock().unwrap();
        authority.revoke_lease();
        authority
            .mark_presented_ready_until(sequence(8), at(1_500_000), at(10))
            .unwrap();
        authority.grant_lease(credentials().lease, at(10)).unwrap();
    }
    assert_eq!(
        native.issue_readiness_ticket_timed(ticket(4), at(10)),
        Err(Refusal::Authority(AuthorityError::StaleLease))
    );
}

#[test]
fn clock_regression_and_expiry_do_not_mint_a_new_readiness_proof() {
    let (_, mut native) = setup(Some(sequence(7)));
    native
        .issue_readiness_ticket_timed(ticket(4), at(100))
        .unwrap();
    assert_eq!(
        native.issue_readiness_ticket_timed(ticket(5), at(99)),
        Err(Refusal::Authority(AuthorityError::ClockRegression))
    );
    assert!(native.monitor().is_revoked());
    assert_eq!(
        native.issue_readiness_ticket_timed(ticket(5), at(101)),
        Err(Refusal::Revoked)
    );
    let (shared, mut native) = setup(Some(sequence(7)));
    assert!(!shared.lock().unwrap().apply_resume_boundary(at(3_000_000)));
    assert_eq!(shared.lock().unwrap().presented_sequence(), None);
    assert!(
        native
            .issue_readiness_ticket_timed(ticket(4), at(3_000_000))
            .is_err()
    );
}
