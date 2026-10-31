//! Policy fixtures for the final native-validation boundary, not OS evidence.
use super::*;
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy)]
enum Change {
    FinishAt(u64),
    Revoke,
    Refuse,
    Panic,
}

struct Boundary {
    inner: Sink,
    now: Rc<Cell<u64>>,
    monitor: InputMonitor,
    change: Change,
    authorizations: usize,
    deadline: Option<HostInstant>,
}

impl Boundary {
    fn new(owner: &InputSession, now: &Rc<Cell<u64>>, change: Change) -> Self {
        Self {
            inner: Sink::default(),
            now: now.clone(),
            monitor: owner.monitor(),
            change,
            authorizations: 0,
            deadline: None,
        }
    }
}

impl InputSink for Boundary {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.inner.prepare(op)
    }

    fn submit(&mut self, op: Operation) -> Submission {
        self.inner.submit(op)
    }

    fn submit_authorized(
        &mut self,
        op: Operation,
        authorize: &mut dyn FnMut() -> Result<HostInstant, Refusal>,
    ) -> Result<Submission, Refusal> {
        // Models a potentially slow native query or a topology/focus refusal
        // after Prepare. All of this must precede the owner's last clock sample.
        match self.change {
            Change::FinishAt(now) => self.now.set(now),
            Change::Revoke => self.monitor.revoke(),
            Change::Refuse => {
                return Ok(Submission::NotSubmitted(PlatformError::GeometryChanged));
            }
            Change::Panic => panic!("injected final native-validation panic"),
        }
        self.authorizations += 1;
        let until = authorize()?;
        self.deadline = Some(until);
        Ok(self.inner.submit_until(op, until))
    }

    fn cancel_prepared(&mut self) {
        self.inner.cancel_prepared();
    }
}

#[test]
fn final_native_validation_cannot_execute_a_ticket_that_expired_while_it_waited() {
    let (mut owner, credentials) = setup();
    let now = Rc::new(Cell::new(0));
    let mut sink = Boundary::new(&owner, &now, Change::FinishAt(1_000_000));
    let result = receipt(
        owner
            .dispatch(
                request(credentials, 0, KeyTransition::Press),
                &mut sink,
                || HostInstant::from_micros(now.get()),
            )
            .unwrap(),
    );
    assert_eq!(result.outcome, InputOutcome::ExpiredBeforeSubmission);
    assert_eq!(result.submitted_operations, 0);
    assert_eq!(sink.authorizations, 1);
    assert_eq!(sink.inner.cancelled, 1);
    assert!(sink.inner.prepared.is_none());
    assert!(sink.inner.effects.is_empty());
    assert_eq!(owner.held_count(), 0);
    let duplicate = receipt(
        owner
            .dispatch(
                request(credentials, 0, KeyTransition::Press),
                &mut sink,
                || HostInstant::from_micros(now.get()),
            )
            .unwrap(),
    );
    assert_eq!(duplicate, result);
    assert_eq!(sink.authorizations, 1);
}

#[test]
fn final_native_validation_precedes_the_successful_clock_and_original_deadline() {
    let (mut owner, credentials) = setup();
    let now = Rc::new(Cell::new(0));
    let mut sink = Boundary::new(&owner, &now, Change::FinishAt(100_000));
    let mut readings = Vec::new();
    let result = receipt(
        owner
            .dispatch(
                request(credentials, 0, KeyTransition::Press),
                &mut sink,
                || {
                    readings.push(now.get());
                    HostInstant::from_micros(now.get())
                },
            )
            .unwrap(),
    );
    assert_eq!(readings, [0, 100_000]);
    assert_eq!(result.outcome, InputOutcome::SubmittedToOs);
    assert_eq!(result.submitted_operations, 1);
    assert_eq!(sink.authorizations, 1);
    assert_eq!(sink.deadline, Some(HostInstant::from_micros(1_000_000)));
    assert_eq!(owner.held_count(), 1);
    assert_eq!(owner.cleanup(&mut sink).remaining, 0);
}

#[test]
fn revocation_during_final_native_validation_prevents_the_press() {
    let (mut owner, credentials) = setup();
    let now = Rc::new(Cell::new(0));
    let mut sink = Boundary::new(&owner, &now, Change::Revoke);
    let result = receipt(
        owner
            .dispatch(
                request(credentials, 0, KeyTransition::Press),
                &mut sink,
                || HostInstant::ORIGIN,
            )
            .unwrap(),
    );
    assert_eq!(result.refusal, Some(Refusal::Revoked));
    assert_eq!(result.submitted_operations, 0);
    assert_eq!(sink.authorizations, 1);
    assert!(sink.inner.effects.is_empty());
    assert_eq!(owner.held_count(), 0);
}

#[test]
fn target_refusal_before_authorization_leaves_no_pessimistic_press() {
    let (mut owner, credentials) = setup();
    let now = Rc::new(Cell::new(0));
    let mut sink = Boundary::new(&owner, &now, Change::Refuse);
    let result = receipt(
        owner
            .dispatch(
                request(credentials, 0, KeyTransition::Press),
                &mut sink,
                || HostInstant::ORIGIN,
            )
            .unwrap(),
    );
    assert_eq!(
        result.refusal,
        Some(Refusal::Platform(PlatformError::GeometryChanged))
    );
    assert_eq!(result.submitted_operations, 0);
    assert_eq!(sink.authorizations, 0);
    assert_eq!(sink.inner.cancelled, 1);
    assert!(sink.inner.effects.is_empty());
    assert_eq!(owner.held_count(), 0);
}

#[test]
fn panic_before_final_authorization_cancels_without_inventing_a_held_key() {
    let (mut owner, credentials) = setup();
    let now = Rc::new(Cell::new(0));
    let mut sink = Boundary::new(&owner, &now, Change::Panic);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = owner.dispatch(
            request(credentials, 0, KeyTransition::Press),
            &mut sink,
            || HostInstant::ORIGIN,
        );
    }));
    assert!(result.is_err());
    assert_eq!(sink.authorizations, 0);
    assert_eq!(sink.inner.cancelled, 1);
    assert!(sink.inner.prepared.is_none());
    assert!(sink.monitor.is_revoked());
    assert!(sink.inner.effects.is_empty());
    assert_eq!(owner.held_count(), 0);
}

#[test]
fn panic_after_final_authorization_preserves_possible_press_cleanup() {
    let (mut owner, credentials) = setup();
    let now = Rc::new(Cell::new(0));
    let mut sink = Boundary::new(&owner, &now, Change::FinishAt(100));
    sink.inner.panic_submit = true;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = owner.dispatch(
            request(credentials, 0, KeyTransition::Press),
            &mut sink,
            || HostInstant::from_micros(now.get()),
        );
    }));
    assert!(result.is_err());
    assert_eq!(sink.authorizations, 1);
    assert_eq!(sink.inner.cancelled, 1);
    assert_eq!(owner.held_count(), 1);
    sink.inner.panic_submit = false;
    assert_eq!(owner.cleanup(&mut sink).remaining, 0);
    assert_eq!(
        sink.inner.effects,
        [Operation::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Release,
        }]
    );
}
