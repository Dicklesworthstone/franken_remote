use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::*,
    input::*,
    input_sequence::InputOutcome,
    input_submission::*,
    time::HostInstant,
};

#[derive(Default)]
struct Sink {
    prepared: Option<Operation>,
    effects: Vec<Operation>,
    cancelled: usize,
    panic_prepare: bool,
    panic_submit: bool,
}
impl InputSink for Sink {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.prepared = Some(op);
        assert!(!self.panic_prepare, "injected preparation panic");
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        assert_eq!(self.prepared, Some(op));
        assert!(!self.panic_submit, "injected submission panic");
        self.effects.push(op);
        Submission::Submitted
    }
    fn cancel_prepared(&mut self) {
        if self.prepared.take().is_some() {
            self.cancelled += 1;
        }
    }
    fn repeat_requires_pair(&self) -> bool {
        true
    }
}
fn setup() -> (InputSession, InputCredentials) {
    let session = RemoteSessionId::from_raw(1);
    let lease = InputLeaseId::from_raw(2);
    let ticket = InputTicketId::from_raw(3);
    let mut a = SessionAuthority::new(session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(HostInstant::ORIGIN).unwrap();
    a.mark_view_ready(HostInstant::ORIGIN).unwrap();
    a.grant_lease(lease, HostInstant::ORIGIN).unwrap();
    a.issue_input_ticket(lease, ticket, HostInstant::ORIGIN)
        .unwrap();
    let c = InputCredentials {
        session,
        lease,
        ticket,
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let owner = InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Repeat),
        HostInstant::ORIGIN,
    )
    .unwrap();
    (owner, c)
}
fn request(c: InputCredentials, sequence: u64, transition: KeyTransition) -> InputRequest<'static> {
    InputRequest {
        credentials: c,
        sequence,
        event: InputEvent::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition,
        },
    }
}
fn receipt(d: Dispatch) -> Receipt {
    match d {
        Dispatch::Completed(r) => r,
        _ => panic!("must report outcome"),
    }
}
#[test]
fn final_expiry_cancels_platform_preparation_without_submission() {
    let (mut owner, c) = setup();
    let mut sink = Sink::default();
    let mut checks = 0;
    let r = receipt(
        owner
            .dispatch(request(c, 0, KeyTransition::Press), &mut sink, || {
                checks += 1;
                HostInstant::from_micros(if checks == 1 { 0 } else { 1_000_000 })
            })
            .unwrap(),
    );
    assert_eq!(r.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(r.submitted_operations, 0);
    assert_eq!(sink.cancelled, 1);
    assert!(sink.prepared.is_none());
    assert_eq!(sink.effects, [] as [Operation; 0]);
    assert_eq!(owner.held_count(), 0);
}
#[test]
fn repeats_are_two_individually_authorized_native_operations() {
    let (mut owner, c) = setup();
    let mut sink = Sink::default();
    let _ = owner
        .dispatch(request(c, 0, KeyTransition::Press), &mut sink, || {
            HostInstant::ORIGIN
        })
        .unwrap();
    let r = receipt(
        owner
            .dispatch(request(c, 1, KeyTransition::Repeat), &mut sink, || {
                HostInstant::ORIGIN
            })
            .unwrap(),
    );
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(r.submitted_operations, 2);
    assert_eq!(
        sink.effects,
        vec![
            Operation::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press
            },
            Operation::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Release
            },
            Operation::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Press
            },
        ]
    );
    assert_eq!(owner.held_count(), 1);
    let duplicate = receipt(
        owner
            .dispatch(request(c, 1, KeyTransition::Repeat), &mut sink, || {
                HostInstant::ORIGIN
            })
            .unwrap(),
    );
    assert_eq!(r, duplicate);
    assert_eq!(sink.effects.len(), 3);
}
#[test]
fn expiry_between_repeat_release_and_press_reports_partial_without_replay() {
    let (mut owner, c) = setup();
    let mut sink = Sink::default();
    let _ = owner
        .dispatch(request(c, 0, KeyTransition::Press), &mut sink, || {
            HostInstant::ORIGIN
        })
        .unwrap();
    let mut checks = 0;
    let r = receipt(
        owner
            .dispatch(request(c, 1, KeyTransition::Repeat), &mut sink, || {
                checks += 1;
                HostInstant::from_micros(if checks == 3 { 1_000_000 } else { 0 })
            })
            .unwrap(),
    );
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.submitted_operations, 1);
    assert_eq!(owner.held_count(), 0);
    assert_eq!(sink.effects.len(), 2);
    assert!(sink.prepared.is_none());
    assert_eq!(sink.cancelled, 3);
    let d = receipt(
        owner
            .dispatch(request(c, 1, KeyTransition::Repeat), &mut sink, || {
                HostInstant::from_micros(1_000_000)
            })
            .unwrap(),
    );
    assert_eq!(r, d);
    assert_eq!(sink.effects.len(), 2);
}
#[test]
fn unwinding_prepare_cancels_preparation_and_fences_input() {
    let (mut owner, c) = setup();
    let mut sink = Sink {
        panic_prepare: true,
        ..Sink::default()
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = owner.dispatch(request(c, 0, KeyTransition::Press), &mut sink, || {
            HostInstant::ORIGIN
        });
    }));
    assert!(outcome.is_err());
    assert_eq!(sink.cancelled, 1);
    assert!(sink.prepared.is_none());
    assert_eq!(sink.effects, [] as [Operation; 0]);
    assert_eq!(owner.held_count(), 0);
    sink.panic_prepare = false;
    assert!(
        owner
            .dispatch(request(c, 1, KeyTransition::Press), &mut sink, || {
                HostInstant::ORIGIN
            })
            .is_err()
    );
}
#[test]
fn unwinding_submit_cancels_preparation_but_preserves_possible_press_cleanup() {
    let (mut owner, c) = setup();
    let mut sink = Sink {
        panic_submit: true,
        ..Sink::default()
    };
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = owner.dispatch(request(c, 0, KeyTransition::Press), &mut sink, || {
            HostInstant::ORIGIN
        });
    }));
    assert!(outcome.is_err());
    assert_eq!(sink.cancelled, 1);
    assert!(sink.prepared.is_none());
    assert_eq!(owner.held_count(), 1);
    sink.panic_submit = false;
    assert_eq!(owner.cleanup(&mut sink).remaining, 0);
    assert_eq!(
        sink.effects,
        vec![Operation::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Release
        }]
    );
}
