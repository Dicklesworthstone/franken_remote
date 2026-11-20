//! The final check's deadline reaches an out-of-thread executor, and executor
//! expiry/fencing are confirmed non-submissions that end the input owner.
use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, SessionAuthority},
    held_state::{HeldState, HeldStateRequest},
    ids::*,
    input::*,
    input_sequence::{InputOutcome, InputSequenceError},
    input_submission::*,
    time::HostInstant,
};
use std::sync::{Arc, Mutex};

/// Records the deadline each submission receives and answers with a scripted
/// executor result. It is a boundary fixture, not native evidence.
#[derive(Default)]
struct Executor {
    prepared: Option<Operation>,
    deadlines: Vec<HostInstant>,
    effects: Vec<Operation>,
    answer: Option<Submission>,
    release_answer: Option<Submission>,
}
impl InputSink for Executor {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.prepared = Some(op);
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        // Without a deadline only releases may reach an executor.
        assert!(op.is_release(), "deadline-free submission of {op:?}");
        assert_eq!(self.prepared.take(), Some(op));
        let answer = self.release_answer.unwrap_or(Submission::Submitted);
        if answer == Submission::Submitted {
            self.effects.push(op);
        }
        answer
    }
    fn submit_until(&mut self, op: Operation, until: HostInstant) -> Submission {
        assert_eq!(self.prepared.take(), Some(op));
        self.deadlines.push(until);
        let answer = self.answer.unwrap_or(Submission::Submitted);
        if answer == Submission::Submitted {
            self.effects.push(op);
        }
        answer
    }
    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
}
/// An in-thread sink implementing only the required methods.
#[derive(Default)]
struct InThread(Vec<Operation>);
impl InputSink for InThread {
    fn prepare(&mut self, _: Operation) -> Result<(), PlatformError> {
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.0.push(op);
        Submission::Submitted
    }
}

fn at(micros: u64) -> HostInstant {
    HostInstant::from_micros(micros)
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
fn grant(view_until: Option<HostInstant>) -> InputSession {
    let c = credentials();
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    match view_until {
        Some(until) => a.mark_view_ready_until(until, at(0)).unwrap(),
        None => a.mark_view_ready(at(0)).unwrap(),
    }
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    InputSession::new(
        a,
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Absolute)
            .with(Capability::Buttons),
        at(0),
    )
    .unwrap()
}
fn press() -> InputEvent<'static> {
    InputEvent::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition: KeyTransition::Press,
    }
}
fn click() -> InputEvent<'static> {
    InputEvent::Button {
        button: PointerButton::Primary,
        pressed: true,
        position: DesktopPoint { x: 10, y: 20 },
        barrier: 0,
    }
}
fn send(
    owner: &mut InputSession,
    sink: &mut impl InputSink,
    sequence: u64,
    event: InputEvent<'_>,
    now: u64,
) -> Result<Dispatch, Refusal> {
    owner.dispatch(
        InputRequest {
            credentials: credentials(),
            sequence,
            event,
        },
        sink,
        || at(now),
    )
}
fn receipt(d: Result<Dispatch, Refusal>) -> Receipt {
    match d {
        Ok(Dispatch::Completed(r)) => r,
        other => panic!("receipt required, got {other:?}"),
    }
}

#[test]
fn every_native_operation_receives_its_own_final_deadline() {
    let mut owner = grant(None);
    let mut sink = Executor::default();
    let r = receipt(send(&mut owner, &mut sink, 0, press(), 100));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    let r = receipt(send(&mut owner, &mut sink, 1, click(), 200));
    assert_eq!(r.submitted_operations, 2);
    // Ticket lifetime (1s) bounds each check; position and press separately.
    assert_eq!(sink.deadlines, [at(1_000_000); 3]);
    // A bounded source deadline earlier than the ticket is what is passed.
    let mut bounded = grant(Some(at(250_000)));
    let mut sink = Executor::default();
    receipt(send(&mut bounded, &mut sink, 0, press(), 10));
    assert_eq!(sink.deadlines, [at(250_000)]);
}

#[test]
fn executor_expiry_is_a_typed_refusal_that_fences_without_held_state() {
    let mut owner = grant(None);
    let monitor = owner.monitor();
    let mut sink = Executor {
        answer: Some(Submission::Expired),
        ..Executor::default()
    };
    let r = receipt(send(&mut owner, &mut sink, 0, press(), 100));
    assert_eq!(r.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(r.refusal, Some(Refusal::ExpiredAtBoundary));
    assert_eq!(r.submitted_operations, 0);
    // The pessimistic press record is restored: nothing to release.
    assert_eq!(owner.held_count(), 0);
    assert_eq!(sink.effects, [] as [Operation; 0]);
    // Expiry is terminal for this owner, never a transparent retry.
    assert!(monitor.is_revoked());
    sink.answer = None;
    assert_eq!(
        send(&mut owner, &mut sink, 1, press(), 200),
        Err(Refusal::Sequence(InputSequenceError::Fenced))
    );
    assert_eq!(sink.deadlines.len(), 1);
    assert_eq!(owner.cleanup(&mut sink).submitted_releases, 0);
    assert_eq!(sink.effects, [] as [Operation; 0]);
}

#[test]
fn executor_fence_reports_cancellation_and_keeps_the_committed_prefix() {
    let mut owner = grant(None);
    let mut sink = Executor::default();
    receipt(send(&mut owner, &mut sink, 0, press(), 100));
    assert_eq!(owner.held_count(), 1);
    // Position submitted; the button press then meets a fenced executor.
    let mut calls = 0;
    let mut fenced = Executor::default();
    let r = receipt(owner.dispatch(
        InputRequest {
            credentials: credentials(),
            sequence: 1,
            event: click(),
        },
        &mut ScriptAfter {
            inner: &mut fenced,
            calls: &mut calls,
        },
        || at(200),
    ));
    assert_eq!(r.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(r.refusal, Some(Refusal::Revoked));
    assert_eq!(r.submitted_operations, 1);
    // The fenced press is not held; the earlier key press still is, and only
    // deadline-free release-only cleanup reaches the executor now.
    assert_eq!(owner.held_count(), 1);
    let cleanup = owner.cleanup(&mut sink);
    assert_eq!((cleanup.submitted_releases, cleanup.remaining), (1, 0));
    assert_eq!(
        sink.effects.last(),
        Some(&Operation::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Release,
        })
    );
    // A cleanup release the executor reports as fenced stays tracked.
    let mut owner = owner_with_press();
    let mut refusing = Executor {
        release_answer: Some(Submission::Fenced),
        ..Executor::default()
    };
    assert_eq!(owner.cleanup(&mut refusing).remaining, 1);
}

/// Submits the first operation, answers `Fenced` for the second.
struct ScriptAfter<'a> {
    inner: &'a mut Executor,
    calls: &'a mut u32,
}
impl InputSink for ScriptAfter<'_> {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.inner.prepare(op)
    }
    fn submit(&mut self, op: Operation) -> Submission {
        self.inner.submit(op)
    }
    fn submit_until(&mut self, op: Operation, until: HostInstant) -> Submission {
        *self.calls += 1;
        self.inner.answer = (*self.calls > 1).then_some(Submission::Fenced);
        self.inner.submit_until(op, until)
    }
    fn cancel_prepared(&mut self) {
        self.inner.cancel_prepared();
    }
}
fn owner_with_press() -> InputSession {
    let mut owner = grant(None);
    receipt(send(&mut owner, &mut Executor::default(), 0, press(), 100));
    owner
}

#[test]
fn reconciliation_release_refused_by_the_executor_is_named_not_assumed() {
    for (answer, refusal) in [
        (Submission::Expired, Refusal::ExpiredAtBoundary),
        (Submission::Fenced, Refusal::Revoked),
    ] {
        let mut owner = owner_with_press();
        let mut sink = Executor {
            release_answer: Some(answer),
            ..Executor::default()
        };
        let report = owner
            .reconcile_held(
                HeldStateRequest {
                    session: credentials().session,
                    lease: credentials().lease,
                    sequence: 1,
                    next_action: 1,
                    held: HeldState::empty(),
                },
                &mut sink,
                || at(150),
            )
            .unwrap();
        assert_eq!(report.outcome, ReconciliationOutcome::Refused);
        assert_eq!(report.refusal, Some(refusal));
        assert_eq!(
            (report.submitted_releases, report.remaining_releases),
            (0, 1)
        );
        assert_eq!(owner.held_count(), 1);
    }
}

#[test]
fn in_thread_sinks_keep_their_direct_submission_and_report_no_local_state() {
    let mut owner = grant(None);
    let mut sink = InThread::default();
    let r = receipt(send(&mut owner, &mut sink, 0, press(), 100));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(sink.0.len(), 1);
    assert!(!sink.locally_revoked());
    assert!(!sink.native_failed());
}

/// `grant(Some(view_until))` on a shared authority, so the test can play the
/// media owner's later readiness report.
fn shared_grant(view_until: HostInstant) -> (Arc<Mutex<SessionAuthority>>, InputSession) {
    let c = credentials();
    let mut a = SessionAuthority::new(c.session, AuthorityPolicy::plan_defaults());
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.mark_view_ready_until(view_until, at(0)).unwrap();
    a.grant_lease(c.lease, at(0)).unwrap();
    a.issue_input_ticket(c.lease, c.ticket, at(0)).unwrap();
    let shared = Arc::new(Mutex::new(a));
    let owner = InputSession::from_shared_authority(
        shared.clone(),
        c,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Absolute),
        at(0),
    )
    .unwrap();
    (shared, owner)
}
fn pointer(x: i32) -> InputEvent<'static> {
    InputEvent::Pointer {
        position: DesktopPoint { x, y: 20 },
    }
}

#[test]
fn a_pointer_on_a_stale_view_is_dropped_and_control_resumes() {
    // Plan 11.3: a stale view suspends input under a held lease; it does not
    // end control. Pointer state already on the wire meets that suspension
    // when a narrowed downlink delays the view's evidence (fr-7al9). Pointer
    // state is latest-wins and fences nothing: it is refused with no effect.
    let (shared, mut owner) = shared_grant(at(250_000));
    let monitor = owner.monitor();
    let mut sink = Executor::default();
    let r = receipt(send(&mut owner, &mut sink, 0, pointer(10), 100));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    let r = receipt(send(&mut owner, &mut sink, 1, pointer(11), 250_000));
    assert_eq!(r.outcome, InputOutcome::RejectedBeforeSubmission);
    assert_eq!(
        r.refusal,
        Some(Refusal::Authority(AuthorityError::ViewUnready))
    );
    assert_eq!(r.submitted_operations, 0);
    assert!(!monitor.is_revoked());
    // The watchdog's suspension service confirms the release; fresh evidence
    // revives the view. The old ticket is gone, which is again only a drop.
    assert_eq!(owner.maintain(at(250_001), &mut sink).remaining, 0);
    shared
        .lock()
        .unwrap()
        .mark_view_ready_until(at(500_000), at(250_002))
        .unwrap();
    let r = receipt(send(&mut owner, &mut sink, 2, pointer(12), 250_003));
    assert_eq!(
        r.refusal,
        Some(Refusal::Authority(AuthorityError::TicketInvalid))
    );
    assert!(!monitor.is_revoked());
    // A fresh ticket resumes pointer input on the same owner and lease.
    let fresh = InputTicketId::from_raw(4);
    owner.issue_ticket(fresh, at(250_004)).unwrap();
    let r = receipt(owner.dispatch(
        InputRequest {
            credentials: InputCredentials {
                ticket: fresh,
                ..credentials()
            },
            sequence: 3,
            event: pointer(13),
        },
        &mut sink,
        || at(250_005),
    ));
    assert_eq!(r.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(sink.effects.len(), 2);
}

#[test]
fn a_stale_view_still_ends_ordered_input_and_an_uncertain_pointer() {
    // A refused ordered action keeps its fence: dependent input must not
    // apply after it, so the owner ends as before.
    let (_, mut owner) = shared_grant(at(250_000));
    let monitor = owner.monitor();
    let r = receipt(send(
        &mut owner,
        &mut Executor::default(),
        0,
        press(),
        250_000,
    ));
    assert_eq!(
        r.refusal,
        Some(Refusal::Authority(AuthorityError::ViewUnready))
    );
    assert!(monitor.is_revoked());
    // A pointer whose native effect is unknown still ends control.
    let (_, mut owner) = shared_grant(at(250_000));
    let monitor = owner.monitor();
    let mut sink = Executor {
        answer: Some(Submission::Unknown),
        ..Executor::default()
    };
    let r = receipt(send(&mut owner, &mut sink, 0, pointer(10), 100));
    assert_eq!(r.outcome, InputOutcome::EffectUnknown);
    assert!(monitor.is_revoked());
    // So does a pointer that meets an expired lease.
    let (_, mut owner) = shared_grant(at(10_000_000));
    let monitor = owner.monitor();
    let r = receipt(send(
        &mut owner,
        &mut Executor::default(),
        0,
        pointer(10),
        3_000_000,
    ));
    assert_eq!(r.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert!(monitor.is_revoked());
}
