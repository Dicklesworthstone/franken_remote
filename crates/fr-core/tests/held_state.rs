use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    held_state::{HeldState, HeldStateRequest},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    time::HostInstant,
};
#[derive(Default)]
struct Sink {
    effects: Vec<Operation>,
    prepared: Option<Operation>,
    release_calls: u16,
    fail_at: Option<u16>,
    unknown: bool,
    panic: bool,
    cancel: Option<RevokeHandle>,
}
impl InputSink for Sink {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        self.prepared = Some(operation);
        if let Some(cancel) = &self.cancel {
            cancel.revoke();
        }
        Ok(())
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        assert_eq!(self.prepared, Some(operation));
        let release = matches!(
            operation,
            Operation::Key {
                transition: KeyTransition::Release,
                ..
            } | Operation::Button { pressed: false, .. }
        );
        if release {
            self.release_calls += 1;
            if self.fail_at == Some(self.release_calls) {
                assert!(!self.panic, "injected release panic");
                return if self.unknown {
                    Submission::Unknown
                } else {
                    Submission::NotSubmitted(PlatformError::Unavailable)
                };
            }
        }
        self.effects.push(operation);
        Submission::Submitted
    }
    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
}
fn key(usage: u16) -> PhysicalKey {
    PhysicalKey::new(usage).unwrap()
}
fn setup() -> (InputSession, InputCredentials, Sink) {
    let credentials = InputCredentials {
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
    let mut authority =
        SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority
        .authorize_observation(HostInstant::ORIGIN)
        .unwrap();
    authority.mark_view_ready(HostInstant::ORIGIN).unwrap();
    authority
        .grant_lease(credentials.lease, HostInstant::ORIGIN)
        .unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, HostInstant::ORIGIN)
        .unwrap();
    let owner = InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Buttons)
            .with(Capability::Absolute),
        HostInstant::ORIGIN,
    )
    .unwrap();
    (owner, credentials, Sink::default())
}
fn press(owner: &mut InputSession, c: InputCredentials, sink: &mut Sink, seq: u64, usage: u16) {
    let Dispatch::Completed(r) = owner
        .dispatch(
            InputRequest {
                credentials: c,
                sequence: seq,
                event: InputEvent::Key {
                    key: key(usage),
                    transition: KeyTransition::Press,
                },
            },
            sink,
            || HostInstant::ORIGIN,
        )
        .unwrap()
    else {
        panic!("receipt expected")
    };
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
}
fn request(
    c: InputCredentials,
    sequence: u64,
    next_action: u64,
    held: HeldState,
) -> HeldStateRequest {
    HeldStateRequest {
        session: c.session,
        lease: c.lease,
        sequence,
        next_action,
        held,
    }
}
#[test]
fn releases_only_missing_remote_state_and_does_not_consume_actions() {
    let (mut owner, c, mut sink) = setup();
    press(&mut owner, c, &mut sink, 0, 4);
    press(&mut owner, c, &mut sink, 1, 0xe1);
    let mut held = HeldState::empty();
    held.set_key(key(0xe1), true);
    held.set_key(key(5), true);
    held.set_button(PointerButton::Primary, true);
    let report = owner
        .reconcile_held(request(c, 0, 2, held), &mut sink, || HostInstant::ORIGIN)
        .unwrap();
    assert_eq!(report.outcome, ReconciliationOutcome::Applied);
    assert_eq!(
        (report.submitted_releases, report.remaining_releases),
        (1, 0)
    );
    assert_eq!(owner.held_count(), 1);
    assert_eq!(sink.effects.len(), 3);
    assert_eq!(
        sink.effects[2],
        Operation::Key {
            key: key(4),
            transition: KeyTransition::Release
        }
    );
    press(&mut owner, c, &mut sink, 2, 4);
    assert_eq!(owner.held_count(), 2);
}
#[test]
fn ticket_expiry_does_not_prevent_release_reconciliation() {
    let (mut owner, c, mut sink) = setup();
    press(&mut owner, c, &mut sink, 0, 4);
    let r = owner
        .reconcile_held(request(c, 0, 1, HeldState::empty()), &mut sink, || {
            HostInstant::from_micros(1_500_000)
        })
        .unwrap();
    assert_eq!(r.outcome, ReconciliationOutcome::Applied);
    assert_eq!(r.submitted_releases, 1);
    assert!(!owner.monitor().is_revoked());
}
#[test]
fn new_epoch_or_old_snapshot_cannot_release_a_newer_press() {
    let (mut owner, c, mut sink) = setup();
    press(&mut owner, c, &mut sink, 0, 4);
    let mut foreign = request(c, 0, 1, HeldState::empty());
    foreign.lease = InputLeaseId::from_raw(9);
    assert_eq!(
        owner
            .reconcile_held(foreign, &mut sink, || HostInstant::ORIGIN)
            .unwrap()
            .outcome,
        ReconciliationOutcome::Ignored
    );
    assert_eq!(
        owner
            .reconcile_held(request(c, 10, 0, HeldState::empty()), &mut sink, || {
                HostInstant::ORIGIN
            })
            .unwrap()
            .outcome,
        ReconciliationOutcome::Ignored
    );
    assert_eq!(owner.held_count(), 1);
    let r = owner
        .reconcile_held(request(c, 0, 1, HeldState::empty()), &mut sink, || {
            HostInstant::ORIGIN
        })
        .unwrap();
    assert_eq!(r.submitted_releases, 1);
    press(&mut owner, c, &mut sink, 1, 4);
    assert_eq!(
        owner
            .reconcile_held(request(c, 0, 2, HeldState::empty()), &mut sink, || {
                HostInstant::ORIGIN
            })
            .unwrap()
            .outcome,
        ReconciliationOutcome::Ignored
    );
    assert_eq!(owner.held_count(), 1);
}
#[test]
fn future_action_barrier_fences_without_skipping_an_action() {
    let (mut owner, c, mut sink) = setup();
    press(&mut owner, c, &mut sink, 0, 4);
    assert!(
        owner
            .reconcile_held(request(c, 0, 2, HeldState::empty()), &mut sink, || {
                HostInstant::ORIGIN
            })
            .is_err()
    );
    assert!(owner.monitor().is_revoked());
    assert_eq!(sink.release_calls, 0);
    assert_eq!(owner.cleanup(&mut sink).remaining, 0);
}
#[test]
fn expired_or_revoked_lease_never_applies_remote_state() {
    let (mut owner, c, mut sink) = setup();
    press(&mut owner, c, &mut sink, 0, 4);
    let r = owner
        .reconcile_held(request(c, 0, 1, HeldState::empty()), &mut sink, || {
            HostInstant::from_micros(3_000_000)
        })
        .unwrap();
    assert_eq!(r.outcome, ReconciliationOutcome::Refused);
    assert_eq!(sink.release_calls, 0);
    assert!(owner.monitor().is_revoked());
    assert_eq!(
        owner
            .reconcile_held(request(c, 1, 1, HeldState::empty()), &mut sink, || {
                HostInstant::from_micros(3_000_000)
            })
            .unwrap()
            .outcome,
        ReconciliationOutcome::Ignored
    );
    assert_eq!(owner.cleanup(&mut sink).remaining, 0);
}
#[test]
fn unknown_and_failed_release_preserve_prefix_and_retained_held_state() {
    for unknown in [false, true] {
        let (mut owner, c, mut sink) = setup();
        press(&mut owner, c, &mut sink, 0, 4);
        press(&mut owner, c, &mut sink, 1, 5);
        sink.fail_at = Some(2);
        sink.unknown = unknown;
        let r = owner
            .reconcile_held(request(c, 0, 2, HeldState::empty()), &mut sink, || {
                HostInstant::ORIGIN
            })
            .unwrap();
        assert_eq!(r.submitted_releases, 1);
        assert_eq!(r.remaining_releases, 1);
        assert_eq!(
            r.outcome,
            if unknown {
                ReconciliationOutcome::EffectUnknown
            } else {
                ReconciliationOutcome::Refused
            }
        );
        assert_eq!(owner.held_count(), 1);
        assert!(owner.monitor().is_revoked());
        assert!(sink.prepared.is_none());
        assert_eq!(owner.cleanup(&mut sink).remaining, 0);
    }
}
#[test]
fn panic_retains_confirmed_prefix_and_uncertain_remainder() {
    let (mut owner, c, mut sink) = setup();
    press(&mut owner, c, &mut sink, 0, 4);
    press(&mut owner, c, &mut sink, 1, 5);
    sink.fail_at = Some(2);
    sink.panic = true;
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| owner.reconcile_held(
            request(c, 0, 2, HeldState::empty()),
            &mut sink,
            || HostInstant::ORIGIN
        )))
        .is_err()
    );
    let r = owner.retained_reconciliation().unwrap();
    assert_eq!(r.outcome, ReconciliationOutcome::EffectUnknown);
    assert_eq!(r.submitted_releases, 1);
    assert_eq!(r.remaining_releases, 1);
    assert!(sink.prepared.is_none());
    assert!(owner.monitor().is_revoked());
    assert_eq!(owner.cleanup(&mut sink).remaining, 0);
}
#[test]
fn revoke_during_preparation_prevents_release_submission_and_cleans_preparation() {
    let (mut owner, c, mut sink) = setup();
    press(&mut owner, c, &mut sink, 0, 4);
    sink.cancel = Some(owner.revoke_handle());
    let r = owner
        .reconcile_held(request(c, 0, 1, HeldState::empty()), &mut sink, || {
            HostInstant::ORIGIN
        })
        .unwrap();
    assert_eq!(r.outcome, ReconciliationOutcome::Refused);
    assert_eq!(r.submitted_releases, 0);
    assert_eq!(sink.release_calls, 0);
    assert!(sink.prepared.is_none());
}
#[test]
fn snapshot_sequence_exhaustion_and_gaps_do_not_wrap_or_touch_action_floor() {
    let (mut owner, c, mut sink) = setup();
    assert_eq!(
        owner
            .reconcile_held(
                request(c, u64::MAX, 0, HeldState::empty()),
                &mut sink,
                || HostInstant::ORIGIN
            )
            .unwrap()
            .outcome,
        ReconciliationOutcome::Applied
    );
    assert_eq!(
        owner
            .reconcile_held(request(c, 0, 0, HeldState::empty()), &mut sink, || {
                HostInstant::ORIGIN
            })
            .unwrap()
            .outcome,
        ReconciliationOutcome::Ignored
    );
    press(&mut owner, c, &mut sink, 0, 4);
}
