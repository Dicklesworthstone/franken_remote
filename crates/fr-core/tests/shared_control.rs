use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    time::HostInstant,
};
use std::sync::{Arc, Mutex, mpsc};
use std::{thread, time::Duration};
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}
fn setup() -> (Arc<Mutex<SessionAuthority>>, InputSession, ControlLease) {
    let c = credentials();
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.mark_view_ready(at(0)).unwrap();
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
    let renewal = native.take_control_lease().unwrap();
    (shared, native, renewal)
}
fn observe(a: &Arc<Mutex<SessionAuthority>>, nonce: u128, now: u64) {
    let mut a = a.lock().unwrap();
    a.issue_observation_challenge(nonce, at(now)).unwrap();
    a.respond_observation_challenge(nonce, at(now)).unwrap();
}
#[derive(Default)]
struct Sink(usize);
impl InputSink for Sink {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, _: Operation) -> Submission {
        self.0 += 1;
        Submission::Submitted
    }
}
fn press(native: &mut InputSession, ticket: InputTicketId, now: u64) -> Receipt {
    let Dispatch::Completed(r) = native
        .dispatch(
            InputRequest {
                credentials: InputCredentials {
                    ticket,
                    ..credentials()
                },
                sequence: 0,
                event: InputEvent::Key {
                    key: PhysicalKey::new(4).unwrap(),
                    transition: KeyTransition::Press,
                },
            },
            &mut Sink::default(),
            || at(now),
        )
        .unwrap()
    else {
        panic!("receipt required")
    };
    r
}
#[test]
fn one_shared_authority_renews_media_and_native_control_beyond_initial_expiry() {
    let (a, mut native, mut renewal) = setup();
    assert!(renewal.uses_authority(&a));
    assert!(native.take_control_lease().is_none());
    observe(&a, 9, 1_000_000);
    assert_eq!(renewal.challenge(10, at(1_000_000)), Ok(at(4_000_000)));
    assert_eq!(renewal.respond(10, at(1_500_000)), Ok(at(4_000_000)));
    let ticket = InputTicketId::from_raw(4);
    native.issue_ticket(ticket, at(2_900_000)).unwrap();
    assert_eq!(
        press(&mut native, ticket, 3_100_000).outcome,
        InputOutcome::SubmittedToOs
    );
    assert_eq!(native.monitor().deadline(at(3_100_000)), Ok(at(4_000_000)));
    a.lock().unwrap().close();
    assert!(renewal.deadline(at(3_100_001)).is_err());
    assert!(native.monitor().is_revoked());
}
#[test]
fn either_observation_or_control_expiry_is_terminal_and_cannot_be_resurrected() {
    let (a, native, mut renewal) = setup();
    renewal.challenge(9, at(1_000_000)).unwrap();
    renewal.respond(9, at(1_500_000)).unwrap();
    assert!(renewal.deadline(at(3_000_000)).is_err());
    observe(&a, 10, 2_000_000);
    assert!(renewal.respond(9, at(3_000_001)).is_err());
    assert!(native.monitor().is_revoked());
    let (a, native, mut renewal) = setup();
    observe(&a, 10, 2_000_000);
    renewal.challenge(11, at(2_000_000)).unwrap();
    assert!(renewal.respond(11, at(3_000_000)).is_err());
    assert!(native.monitor().is_revoked());
}
#[test]
fn overtaken_samples_use_conservative_serialization_but_own_clock_regression_stops() {
    let (a, mut native, mut renewal) = setup();
    a.lock()
        .unwrap()
        .authorize_observation_delivery(at(100))
        .unwrap();
    assert_eq!(
        press(&mut native, credentials().ticket, 50).outcome,
        InputOutcome::SubmittedToOs
    );
    assert_eq!(renewal.challenge(9, at(75)), Ok(at(3_000_100)));
    assert_eq!(renewal.respond(9, at(90)), Ok(at(3_000_100)));
    assert_eq!(
        renewal.deadline(at(89)),
        Err(Refusal::Authority(AuthorityError::ClockRegression))
    );
    assert!(native.monitor().is_revoked());
}
#[test]
fn native_regression_is_not_hidden_by_a_newer_shared_clock() {
    let (a, mut native, _renewal) = setup();
    native
        .issue_ticket(InputTicketId::from_raw(4), at(100))
        .unwrap();
    a.lock()
        .unwrap()
        .authorize_observation_delivery(at(200))
        .unwrap();
    assert_eq!(
        native.issue_ticket(InputTicketId::from_raw(5), at(99)),
        Err(Refusal::Authority(AuthorityError::ClockRegression))
    );
}
#[test]
fn overtaken_ticket_metadata_uses_the_same_issue_time_as_policy() {
    let (a, mut native, _renewal) = setup();
    a.lock()
        .unwrap()
        .authorize_observation_delivery(at(100))
        .unwrap();
    let (issued, until) = native
        .issue_ticket_timed(InputTicketId::from_raw(4), at(50))
        .unwrap();
    assert_eq!(issued, at(100));
    assert_eq!(until, at(1_000_100));
}
#[test]
fn dropping_either_owner_and_replacement_authority_cannot_keep_old_control() {
    let (a, mut native, renewal) = setup();
    drop(renewal);
    assert!(native.monitor().is_revoked());
    assert!(native.take_control_lease().is_none());
    assert!(
        a.lock()
            .unwrap()
            .authorize_observation_delivery(at(1))
            .is_ok()
    );
    let (_, native, mut renewal) = setup();
    drop(native);
    assert!(renewal.challenge(10, at(1)).is_err());
    let (a, native, mut renewal) = setup();
    let (_, _, other) = setup();
    assert!(!other.uses_authority(&a));
    let mut shared = a.lock().unwrap();
    shared.revoke_lease();
    shared
        .grant_lease(InputLeaseId::from_raw(99), at(1))
        .unwrap();
    drop(shared);
    assert!(renewal.challenge(10, at(2)).is_err());
    assert!(native.monitor().is_revoked());
}
#[test]
fn shared_attachment_cannot_grant_control_or_bypass_ready_and_ticket_checks() {
    let a = Arc::new(Mutex::new(SessionAuthority::new(
        credentials().session,
        AuthorityPolicy::plan_defaults(),
    )));
    assert!(
        InputSession::from_shared_authority(
            a,
            credentials(),
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
            at(0)
        )
        .is_err()
    );
    let (a, _native, _renewal) = setup();
    let bad = InputCredentials {
        session: RemoteSessionId::from_raw(99),
        ..credentials()
    };
    assert!(matches!(
        InputSession::from_shared_authority(
            a,
            bad,
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
            at(0)
        ),
        Err(Refusal::StaleSession)
    ));
}
struct Blocked {
    entered: mpsc::SyncSender<()>,
    resume: mpsc::Receiver<()>,
    effects: usize,
    in_submit: bool,
}
impl InputSink for Blocked {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        if !self.in_submit {
            self.entered.send(()).unwrap();
            self.resume.recv().unwrap();
        }
        Ok(())
    }
    fn submit(&mut self, _: Operation) -> Submission {
        if self.in_submit {
            self.entered.send(()).unwrap();
            self.resume.recv().unwrap();
        }
        self.effects += 1;
        Submission::Submitted
    }
}
fn blocked(in_submit: bool) {
    let (a, mut native, mut renewal) = setup();
    let (entered, receive) = mpsc::sync_channel(1);
    let (send, resume) = mpsc::sync_channel(1);
    let native_thread = thread::spawn(move || {
        let mut sink = Blocked {
            entered,
            resume,
            effects: 0,
            in_submit,
        };
        let result = native
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
                || at(10),
            )
            .unwrap();
        (result, sink.effects)
    });
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    let (done_tx, done_rx) = mpsc::sync_channel(1);
    let control_thread = thread::spawn(move || {
        observe(&a, 1, 1_000_000);
        renewal.challenge(2, at(1_000_000)).unwrap();
        renewal.respond(2, at(1_500_000)).unwrap();
        let live = renewal.deadline(at(3_500_000));
        renewal.stop();
        done_tx.send(live).unwrap();
    });
    let result = done_rx.recv_timeout(Duration::from_secs(2));
    send.send(()).unwrap();
    let (dispatch, effects) = native_thread.join().unwrap();
    control_thread.join().unwrap();
    assert_eq!(result.unwrap(), Ok(at(4_000_000)));
    let Dispatch::Completed(receipt) = dispatch else {
        panic!("receipt")
    };
    assert_eq!(effects, usize::from(in_submit));
    assert_eq!(
        receipt.outcome,
        if in_submit {
            InputOutcome::SubmittedToOs
        } else {
            InputOutcome::CancelledBeforeSubmission
        }
    );
}
#[test]
fn renewal_and_revoke_progress_while_native_preparation_is_blocked() {
    blocked(false);
}
#[test]
fn renewal_and_revoke_progress_while_native_submission_is_blocked() {
    blocked(true);
}

#[test]
fn shared_native_attachment_is_single_use_even_after_owner_stop_and_drop() {
    let (a, mut native, renewal) = setup();
    let attach = || {
        InputSession::from_shared_authority(
            a.clone(),
            credentials(),
            InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
            Capabilities::default(),
            at(0),
        )
    };
    assert!(matches!(
        attach(),
        Err(Refusal::Authority(AuthorityError::ControllerBusy))
    ));
    native.revoke();
    assert!(matches!(
        attach(),
        Err(Refusal::Authority(AuthorityError::ControllerBusy))
    ));
    drop(native);
    drop(renewal);
    assert!(matches!(
        attach(),
        Err(Refusal::Authority(AuthorityError::ControllerBusy))
    ));
}
#[test]
fn replaced_lease_with_reused_numeric_id_never_revalidates_the_old_native_owner() {
    let (a, native, mut old) = setup();
    {
        let mut a = a.lock().unwrap();
        a.revoke_lease();
        a.grant_lease(credentials().lease, at(1)).unwrap();
        a.issue_input_ticket(credentials().lease, credentials().ticket, at(1))
            .unwrap();
    }
    let mut replacement = InputSession::from_shared_authority(
        a,
        credentials(),
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(Capability::Keys),
        at(1),
    )
    .unwrap();
    let mut fresh = replacement.take_control_lease().unwrap();
    assert!(old.challenge(10, at(2)).is_err());
    assert!(native.monitor().deadline(at(2)).is_err());
    drop(native);
    drop(old);
    assert!(fresh.challenge(11, at(2)).is_ok());
    assert!(fresh.respond(11, at(3)).is_ok());
    assert_eq!(
        press(&mut replacement, credentials().ticket, 4).outcome,
        InputOutcome::SubmittedToOs
    );
}

#[test]
fn original_grant_publication_checks_ticket_and_exact_native_lifetime() {
    let (a, native, renewal) = setup();
    let monitor = native.monitor();
    let c = credentials();
    assert_eq!(monitor.authorize_ticket(c.ticket, at(0)), Ok(()));
    assert_eq!(monitor.authorize_ticket(c.ticket, at(999_999)), Ok(()));
    assert_eq!(
        monitor.authorize_ticket(c.ticket, at(1_000_000)),
        Err(Refusal::Authority(AuthorityError::TicketExpired))
    );
    // An expired ticket does not by itself claim native cleanup or revoke keys.
    assert!(!monitor.is_revoked());
    {
        let mut a = a.lock().unwrap();
        a.revoke_lease();
        a.grant_lease(c.lease, at(1_000_001)).unwrap();
        a.issue_input_ticket(c.lease, c.ticket, at(1_000_001))
            .unwrap();
    }
    assert_eq!(
        monitor.authorize_ticket(c.ticket, at(1_000_002)),
        Err(Refusal::Authority(AuthorityError::StaleLease))
    );
    drop(renewal);
    assert_eq!(
        monitor.authorize_ticket(c.ticket, at(1_000_002)),
        Err(Refusal::Revoked)
    );
}
#[test]
fn grant_publication_does_not_regress_shared_time_or_restore_invalidated_tickets() {
    let (a, native, _renewal) = setup();
    let monitor = native.monitor();
    let c = credentials();
    a.lock()
        .unwrap()
        .authorize_observation_delivery(at(100))
        .unwrap();
    assert_eq!(monitor.authorize_ticket(c.ticket, at(50)), Ok(()));
    {
        let mut a = a.lock().unwrap();
        a.mark_view_stale();
        a.mark_view_ready(at(101)).unwrap();
    }
    assert_eq!(
        monitor.authorize_ticket(c.ticket, at(102)),
        Err(Refusal::Authority(AuthorityError::TicketInvalid))
    );
}
