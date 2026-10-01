//! Plan 11.3: a stale view SUSPENDS a held lease's input. The native owner
//! releases every remotely held key and button without a new input event, and
//! only that confirmed release lets fresh evidence revive input, with a new
//! ticket and the same, unrenewed lease.
use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, ControlStatus, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    time::HostInstant,
};
use std::sync::{Arc, Mutex};
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn credentials(ticket: InputTicketId) -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket,
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
/// Evidence-bounded view until 250 ms, lease held, key 4 pressed at 100 ms.
fn pressed(sink: &mut Sink) -> (Arc<Mutex<SessionAuthority>>, InputSession) {
    let c = credentials(InputTicketId::from_raw(3));
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.require_view_evidence(at(0)).unwrap();
    a.mark_view_ready_until(at(250_000), at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    let shared = Arc::new(Mutex::new(a));
    let mut native = InputSession::from_shared_authority(
        shared.clone(),
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(Capability::Keys),
        at(0),
    )
    .unwrap();
    assert_eq!(
        key(
            &mut native,
            sink,
            c.ticket,
            0,
            KeyTransition::Press,
            100_000
        ),
        InputOutcome::SubmittedToOs
    );
    assert_eq!(native.held_count(), 1);
    (shared, native)
}
#[derive(Default)]
struct Sink {
    presses: usize,
    releases: usize,
    refuse_releases: bool,
}
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        if operation.is_release() {
            if self.refuse_releases {
                return Submission::NotSubmitted(PlatformError::Unavailable);
            }
            self.releases += 1;
        } else {
            self.presses += 1;
        }
        Submission::Submitted
    }
}
fn key(
    native: &mut InputSession,
    sink: &mut Sink,
    ticket: InputTicketId,
    sequence: u64,
    transition: KeyTransition,
    now: u64,
) -> InputOutcome {
    match native.dispatch(
        InputRequest {
            credentials: credentials(ticket),
            sequence,
            event: InputEvent::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition,
            },
        },
        sink,
        || at(now),
    ) {
        Ok(Dispatch::Completed(receipt)) => receipt.outcome,
        other => panic!("{other:?}"),
    }
}
#[test]
fn a_lapse_releases_held_keys_without_revoking_and_only_then_allows_revival() {
    let mut sink = Sink::default();
    let (shared, mut native) = pressed(&mut sink);
    let lease_until = shared.lock().unwrap().lease_deadline().unwrap();
    // Before the lapse, idle maintenance does nothing.
    assert_eq!(
        native.maintain(at(249_999), &mut sink).submitted_releases,
        0
    );
    // The view deadline passes with no report: maintenance releases the held
    // key WITHOUT a new input event, and does not revoke the lease.
    let cleanup = native.maintain(at(250_000), &mut sink);
    assert_eq!((cleanup.submitted_releases, cleanup.remaining), (1, 0));
    assert_eq!(sink.releases, 1);
    assert!(!native.monitor().is_revoked());
    assert_eq!(
        native.monitor().status(at(250_000)),
        Ok(ControlStatus::Suspended {
            until: lease_until,
            released: true
        })
    );
    // The old ticket is dead; fresh evidence revives readiness...
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(600_000), at(260_000))
        .unwrap();
    // ...and only a new ticket authorizes input again, on the same lease.
    let fresh = InputTicketId::from_raw(4);
    native.issue_ticket(fresh, at(270_000)).unwrap();
    assert_eq!(
        key(
            &mut native,
            &mut sink,
            fresh,
            1,
            KeyTransition::Press,
            280_000
        ),
        InputOutcome::SubmittedToOs
    );
    assert_eq!(sink.presses, 2);
    assert_eq!(
        shared.lock().unwrap().lease_deadline().unwrap(),
        lease_until,
        "revival never renews the lease"
    );
}
#[test]
fn a_release_that_did_not_happen_keeps_revival_refused() {
    let mut sink = Sink::default();
    let (shared, mut native) = pressed(&mut sink);
    sink.refuse_releases = true;
    let cleanup = native.maintain(at(250_000), &mut sink);
    assert_eq!((cleanup.submitted_releases, cleanup.remaining), (0, 1));
    assert_eq!(
        shared
            .lock()
            .unwrap()
            .mark_view_ready_until(at(600_000), at(260_000)),
        Err(AuthorityError::ReleasePending)
    );
    // A later maintenance turn that does release lets revival proceed.
    sink.refuse_releases = false;
    let cleanup = native.maintain(at(270_000), &mut sink);
    assert_eq!((cleanup.submitted_releases, cleanup.remaining), (1, 0));
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(600_000), at(280_000))
        .unwrap();
}
#[test]
fn an_input_event_during_the_suspension_is_refused_not_queued() {
    let mut sink = Sink::default();
    let (_, mut native) = pressed(&mut sink);
    let _ = native.maintain(at(250_000), &mut sink);
    let result = native.dispatch(
        InputRequest {
            credentials: credentials(InputTicketId::from_raw(3)),
            sequence: 1,
            event: InputEvent::Key {
                key: PhysicalKey::new(5).unwrap(),
                transition: KeyTransition::Press,
            },
        },
        &mut sink,
        || at(260_000),
    );
    assert!(
        !matches!(
            result,
            Ok(Dispatch::Completed(Receipt {
                outcome: InputOutcome::SubmittedToOs,
                ..
            }))
        ),
        "{result:?}"
    );
    assert_eq!(sink.presses, 1, "no press reached the OS while suspended");
}
#[test]
fn lease_expiry_during_the_suspension_revokes_and_cleans_up() {
    let mut sink = Sink::default();
    let (_, mut native) = pressed(&mut sink);
    sink.refuse_releases = true;
    let _ = native.maintain(at(250_000), &mut sink);
    assert!(!native.monitor().is_revoked());
    // Observation (3 s from 0) ends while suspended: terminal, and cleanup
    // still releases the key once the platform accepts it.
    sink.refuse_releases = false;
    let cleanup = native.maintain(at(3_000_000), &mut sink);
    assert!(native.monitor().is_revoked());
    assert_eq!(cleanup.remaining, 0);
    assert_eq!(sink.releases, 1);
}
/// One key press decided on the view of `recovery`, with `ticket`.
fn key_at(
    native: &mut InputSession,
    sink: &mut Sink,
    ticket: InputTicketId,
    recovery: RecoveryGeneration,
    sequence: u64,
    now: u64,
) -> Receipt {
    let mut credentials = credentials(ticket);
    credentials.view.recovery = recovery;
    match native.dispatch(
        InputRequest {
            credentials,
            sequence,
            event: InputEvent::Key {
                key: PhysicalKey::new(5).unwrap(),
                transition: KeyTransition::Press,
            },
        },
        sink,
        || at(now),
    ) {
        Ok(Dispatch::Completed(receipt)) => receipt,
        other => panic!("{other:?}"),
    }
}
/// A reference recovery: its admission fences the view (a suspension that
/// releases the held key), the media owner installs the next generation, and
/// that generation's first presented evidence revives readiness.
fn recovered(
    sink: &mut Sink,
) -> (
    Arc<Mutex<SessionAuthority>>,
    InputSession,
    RecoveryGeneration,
) {
    let (shared, mut native) = pressed(sink);
    let lease_until = shared.lock().unwrap().lease_deadline().unwrap();
    let next = RecoveryGeneration::INITIAL.next().unwrap();
    shared.lock().unwrap().mark_view_stale();
    let cleanup = native.maintain(at(110_000), sink);
    assert_eq!((cleanup.submitted_releases, cleanup.remaining), (1, 0));
    shared.lock().unwrap().advance_recovery(next).unwrap();
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(400_000), at(120_000))
        .unwrap();
    assert_eq!(
        shared.lock().unwrap().lease_deadline().unwrap(),
        lease_until,
        "a recovery never renews the lease"
    );
    (shared, native, next)
}
#[test]
fn a_recovery_generation_advances_only_on_a_fenced_view_and_only_forward() {
    let mut sink = Sink::default();
    let (shared, _native) = pressed(&mut sink);
    let next = RecoveryGeneration::INITIAL.next().unwrap();
    let mut a = shared.lock().unwrap();
    assert!(
        matches!(
            a.advance_recovery(next),
            Err(AuthorityError::InvalidState { .. })
        ),
        "a ready view cannot change generation under live input"
    );
    a.mark_view_stale();
    a.advance_recovery(next).unwrap();
    assert_eq!(a.recovery_generation(), Some(next));
    assert_eq!(
        a.advance_recovery(next),
        Err(AuthorityError::StaleGeneration)
    );
    assert_eq!(
        a.advance_recovery(RecoveryGeneration::INITIAL),
        Err(AuthorityError::StaleGeneration)
    );
}
#[test]
fn input_resumes_on_the_recovered_generation_with_the_same_lease() {
    let mut sink = Sink::default();
    let (_shared, mut native, next) = recovered(&mut sink);
    // The old ticket was invalidated; a new one names the new generation.
    let fresh = InputTicketId::from_raw(4);
    native.issue_ticket(fresh, at(130_000)).unwrap();
    assert_eq!(native.ticket_credentials(fresh).view.recovery, next);
    let receipt = key_at(&mut native, &mut sink, fresh, next, 1, 140_000);
    assert_eq!(receipt.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(sink.presses, 2);
    assert!(!native.monitor().is_revoked());
}
#[test]
fn an_action_decided_on_the_old_generation_is_refused_even_with_a_new_ticket() {
    let mut sink = Sink::default();
    let (_shared, mut native, _) = recovered(&mut sink);
    let fresh = InputTicketId::from_raw(4);
    native.issue_ticket(fresh, at(130_000)).unwrap();
    let receipt = key_at(
        &mut native,
        &mut sink,
        fresh,
        RecoveryGeneration::INITIAL,
        1,
        140_000,
    );
    assert_eq!(receipt.refusal, Some(Refusal::StaleView));
    assert_eq!(sink.presses, 1, "only the original press reached the OS");
}
#[test]
fn an_old_generation_action_is_refused_with_a_ticket_issued_at_the_authority() {
    let mut sink = Sink::default();
    let (shared, mut native, _) = recovered(&mut sink);
    // A ticket issued on the shared authority itself, not through this native
    // owner (as the initial grant's is): submission still checks the action's
    // view against the authority's current recovery generation.
    let fresh = InputTicketId::from_raw(4);
    shared
        .lock()
        .unwrap()
        .issue_input_ticket(InputLeaseId::from_raw(2), fresh, at(130_000))
        .unwrap();
    let receipt = key_at(
        &mut native,
        &mut sink,
        fresh,
        RecoveryGeneration::INITIAL,
        1,
        140_000,
    );
    assert_eq!(receipt.refusal, Some(Refusal::StaleView));
    assert_eq!(sink.presses, 1, "only the original press reached the OS");
}
