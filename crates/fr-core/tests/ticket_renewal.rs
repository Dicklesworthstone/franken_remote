use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, MAX_LIVE_INPUT_TICKETS, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    time::HostInstant,
};
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn setup() -> (SessionAuthority, InputCredentials) {
    let c = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.mark_view_ready(at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    (a, c)
}
#[test]
fn rollover_preserves_in_flight_tickets_but_never_extends_them() {
    let (mut a, c) = setup();
    let next = InputTicketId::from_raw(4);
    assert_eq!(
        a.issue_input_ticket(c.lease, next, at(250_000)),
        Ok(at(1_250_000))
    );
    assert_eq!(
        a.authorize_submission(c.lease, c.ticket, at(999_999)),
        Ok(())
    );
    assert_eq!(
        a.authorize_submission(c.lease, c.ticket, at(1_000_000)),
        Err(AuthorityError::TicketExpired)
    );
    assert_eq!(a.authorize_submission(c.lease, next, at(1_000_000)), Ok(()));
    assert_eq!(
        a.authorize_submission(c.lease, next, at(1_250_000)),
        Err(AuthorityError::TicketExpired)
    );
}
#[test]
fn full_ring_refuses_without_evicting_live_tickets_then_reuses_expired_slots() {
    let (mut a, c) = setup();
    for id in 4..(3 + MAX_LIVE_INPUT_TICKETS as u128) {
        a.issue_input_ticket(c.lease, InputTicketId::from_raw(id), at(1))
            .unwrap();
    }
    assert_eq!(
        a.issue_input_ticket(c.lease, InputTicketId::from_raw(100), at(2)),
        Err(AuthorityError::TicketCapacity)
    );
    for id in 3..(3 + MAX_LIVE_INPUT_TICKETS as u128) {
        assert_eq!(
            a.authorize_submission(c.lease, InputTicketId::from_raw(id), at(999_999)),
            Ok(())
        );
    }
    a.issue_input_ticket(c.lease, InputTicketId::from_raw(100), at(1_000_000))
        .unwrap();
    assert_eq!(
        a.authorize_submission(c.lease, c.ticket, at(1_000_000)),
        Err(AuthorityError::TicketInvalid)
    );
    assert_eq!(
        a.authorize_submission(c.lease, InputTicketId::from_raw(4), at(1_000_000)),
        Ok(())
    );
}
#[test]
fn invalid_id_or_duplicate_never_slides_a_deadline_or_consumes_capacity() {
    let (mut a, c) = setup();
    assert_eq!(
        a.issue_input_ticket(c.lease, InputTicketId::from_raw(0), at(1)),
        Err(AuthorityError::TicketInvalid)
    );
    a.issue_input_ticket(c.lease, InputTicketId::from_raw(4), at(2))
        .unwrap();
    assert_eq!(
        a.issue_input_ticket(c.lease, c.ticket, at(3)),
        Err(AuthorityError::TicketInvalid)
    );
    assert_eq!(
        a.authorize_submission(c.lease, c.ticket, at(1_000_000)),
        Err(AuthorityError::TicketExpired)
    );
}
#[test]
fn view_suspend_revoke_and_foreign_lease_invalidate_the_entire_set() {
    let (mut a, c) = setup();
    a.issue_input_ticket(c.lease, InputTicketId::from_raw(4), at(1))
        .unwrap();
    assert_eq!(
        a.invalidate_input_tickets(InputLeaseId::from_raw(99)),
        Err(AuthorityError::StaleLease)
    );
    assert_eq!(a.authorize_submission(c.lease, c.ticket, at(2)), Ok(()));
    a.mark_view_stale();
    a.mark_view_ready(at(3)).unwrap();
    for id in [3, 4] {
        assert_eq!(
            a.authorize_submission(c.lease, InputTicketId::from_raw(id), at(3)),
            Err(AuthorityError::TicketInvalid)
        );
    }
    a.issue_input_ticket(c.lease, InputTicketId::from_raw(5), at(4))
        .unwrap();
    a.invalidate_for_suspend();
    assert!(
        a.authorize_submission(c.lease, InputTicketId::from_raw(5), at(4))
            .is_err()
    );
    let (mut a, c) = setup();
    a.revoke_lease();
    assert!(a.authorize_submission(c.lease, c.ticket, at(1)).is_err());
}
#[test]
fn every_ticket_is_capped_by_both_authorities_even_after_renewal() {
    let (mut a, c) = setup();
    let old = InputTicketId::from_raw(8);
    a.issue_input_ticket(c.lease, old, at(2_800_000)).unwrap();
    a.issue_observation_challenge(20, at(2_800_001)).unwrap();
    a.issue_control_challenge(21, at(2_800_001)).unwrap();
    a.respond_observation_challenge(20, at(2_900_000)).unwrap();
    a.respond_control_challenge(c.lease, 21, at(2_900_000))
        .unwrap();
    assert_eq!(
        a.authorize_submission(c.lease, old, at(3_000_000)),
        Err(AuthorityError::TicketExpired)
    );
    assert_eq!(
        a.issue_input_ticket(c.lease, InputTicketId::from_raw(9), at(3_000_000)),
        Ok(at(4_000_000))
    );
}
#[derive(Default)]
struct Sink {
    calls: usize,
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, _: Operation) -> Submission {
        self.calls += 1;
        Submission::Submitted
    }
}
fn owner() -> (InputSession, InputCredentials) {
    let (a, c) = setup();
    (
        InputSession::new(
            a,
            c,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default()
                .with(Capability::Absolute)
                .with(Capability::Relative)
                .with(Capability::Keys),
            at(0),
        )
        .unwrap(),
        c,
    )
}
#[test]
fn native_submission_accepts_original_ticket_after_rollover_without_replaying_actions() {
    let (mut owner, c) = owner();
    let mut sink = Sink::default();
    owner
        .issue_ticket(InputTicketId::from_raw(4), at(100))
        .unwrap();
    let request = InputRequest {
        credentials: c,
        sequence: 0,
        event: InputEvent::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Press,
        },
    };
    let Dispatch::Completed(receipt) = owner.dispatch(request, &mut sink, || at(200)).unwrap()
    else {
        panic!("missing receipt")
    };
    assert_eq!(receipt.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(sink.calls, 1);
    let duplicate = owner.dispatch(request, &mut sink, || at(201)).unwrap();
    assert_eq!(duplicate, Dispatch::Completed(receipt));
    assert_eq!(sink.calls, 1);
}
#[test]
fn mode_transition_invalidates_all_pretransition_tickets_not_just_the_latest() {
    let (mut owner, c) = owner();
    let mut sink = Sink::default();
    owner
        .issue_ticket(InputTicketId::from_raw(4), at(1))
        .unwrap();
    let mode = owner
        .dispatch(
            InputRequest {
                credentials: c,
                sequence: 0,
                event: InputEvent::Mode {
                    mode: PointerMode::Relative,
                    epoch: 1,
                },
            },
            &mut sink,
            || at(2),
        )
        .unwrap();
    assert!(matches!(mode, Dispatch::Completed(r) if r.outcome == InputOutcome::AppliedLocally));
    owner
        .issue_ticket(InputTicketId::from_raw(5), at(3))
        .unwrap();
    let Dispatch::Completed(r) = owner
        .dispatch(
            InputRequest {
                credentials: c,
                sequence: 1,
                event: InputEvent::Key {
                    key: PhysicalKey::new(4).unwrap(),
                    transition: KeyTransition::Press,
                },
            },
            &mut sink,
            || at(4),
        )
        .unwrap()
    else {
        panic!("missing receipt")
    };
    assert_ne!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(sink.calls, 0);
}
