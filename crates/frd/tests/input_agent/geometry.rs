//! The actual native-thread agent with an explicit geometry-refusing sink.
//! Submitted prefixes and held-input cleanup remain owned by that one agent.
use super::*;

struct Resized(Sink);
impl InputSink for Resized {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        let release = matches!(
            operation,
            Operation::Key {
                transition: KeyTransition::Release,
                ..
            }
        );
        if !release && self.0.trace.effects.lock().unwrap().len() >= 2 {
            return Err(PlatformError::GeometryChanged);
        }
        self.0.prepare(operation)
    }
    fn submit(&mut self, operation: Operation) -> Submission {
        self.0.submit(operation)
    }
}

#[test]
fn geometry_during_text_keeps_its_confirmed_prefix_and_releases_the_old_lease() {
    let rt = runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let trace = trace();
    let factory = trace.clone();
    let seat = Seat::default();
    let (mut agent, driver) = seat
        .start(
            cx.clone(),
            session(&cx, 3_000_000),
            route(),
            move || Ok(Resized(Sink::new(factory))),
            |sink| clean(&mut sink.0),
        )
        .unwrap();
    let runner = Runner::start(rt, driver);
    agent
        .submit(&bytes(0, key(true)), InputDelivery::Reliable)
        .unwrap();
    assert_eq!(
        submitted(reply(&mut agent)).outcome,
        InputOutcome::SubmittedToOs
    );
    agent
        .submit(&bytes(1, InputEvent::Text("ab")), InputDelivery::Reliable)
        .unwrap();
    let receipt = submitted(reply(&mut agent));
    assert_eq!(receipt.outcome, InputOutcome::PartiallySubmittedToOs);
    assert_eq!(receipt.submitted_operations, 1);
    assert_eq!(
        receipt.refusal,
        Some(Refusal::Platform(PlatformError::GeometryChanged))
    );
    assert!(agent.control().is_stopped());
    assert_eq!(agent.control().reason(), Some(StopReason::GeometryChanged));
    assert_eq!(
        agent.submit(&bytes(2, key(true)), InputDelivery::Reliable),
        Err(Error::Stopped)
    );
    let shutdown = runner.finish();
    assert_eq!(shutdown.reason, StopReason::GeometryChanged);
    assert!(shutdown.handoff_safe());
    assert!(!trace.held.load(Ordering::Acquire));
    // Original key, exactly one text scalar, and the key's original release.
    assert_eq!(trace.effects.lock().unwrap().len(), 3);
    assert_eq!(trace.drops.load(Ordering::Acquire), 1);
    assert!(!seat.is_occupied());
}
