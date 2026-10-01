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
