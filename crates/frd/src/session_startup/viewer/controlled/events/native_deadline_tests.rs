//! The production queue preserves a native producer's already spent lifetime.
use super::*;
use fr_core::ids::{
    CodecConfigurationGeneration, DisplayGeometryGeneration, RecoveryGeneration,
    ViewportMappingGeneration,
};

fn view() -> InputView {
    InputView {
        geometry: DisplayGeometryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}
fn receiver() -> Receiver {
    let context = TextState::new(view(), 0, false).unwrap();
    Receiver::new(
        Arc::new(Mutex::new(Queue {
            events: VecDeque::new(),
            last_sample: ClientInstant(7),
            context,
            text_ready: true,
        })),
        context,
        true,
    )
}
fn key(usage: u16, transition: KeyTransition) -> Event {
    Event::Key {
        key: PhysicalKey::new(usage).unwrap(),
        transition,
    }
}
fn capture(receiver: &Receiver, event: Event, until: u64) {
    receiver
        .queue
        .lock()
        .unwrap()
        .push_bounded(event, ClientInstant(7), None, Some(ClientInstant(until)))
        .unwrap();
}

#[test]
fn an_expired_native_modifier_cannot_rebind_or_send_its_dependent_letter() {
    for already_pending in [false, true] {
        let mut receiver = receiver();
        capture(&receiver, key(0xE1, KeyTransition::Press), 10);
        capture(&receiver, key(0x04, KeyTransition::Press), 20);
        receiver.expire(ClientInstant(9), &|_| false).unwrap();
        if already_pending {
            assert!(receiver.front(view(), ClientInstant(9)).unwrap().is_some());
        }
        assert_eq!(
            receiver.expire(ClientInstant(10), &|_| false),
            Err(Error::Expired)
        );
        // A newer letter did not renew the old modifier, and the failing
        // owner cannot dispatch another key by calling maintenance again.
        assert_eq!(
            receiver.expire(ClientInstant(11), &|_| false),
            Err(Error::Expired)
        );
        assert_eq!(
            receiver.dropped(),
            0,
            "must fail the source, not skip its modifier"
        );
    }
}

#[test]
fn an_expired_native_release_remains_cleanup_only_for_an_actual_hold() {
    let mut receiver = receiver();
    capture(&receiver, key(0x04, KeyTransition::Release), 9);
    let held = |held| matches!(held, Held::Key(key) if key.usage() == 0x04);
    receiver.expire(ClientInstant(10), &held).unwrap();
    let captured = receiver.front(view(), ClientInstant(10)).unwrap().unwrap();
    assert_eq!(captured.sampled, ClientInstant(7));
    assert_eq!(captured.send_until, Some(ClientInstant(9)));
    assert!(matches!(
        captured.event,
        Event::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));
    receiver.expire(ClientInstant(11), &held).unwrap();
    // Losing the original hold never turns the same expired action back
    // into new input (nor gives it the deadline of another queued event).
    assert_eq!(
        receiver.expire(ClientInstant(11), &|_| false),
        Err(Error::Expired)
    );
}

#[test]
fn native_bounds_leave_existing_text_completion_refusal_unchanged() {
    let mut receiver = receiver();
    capture(
        &receiver,
        Event::Text(CommittedText::new("commit").unwrap()),
        9,
    );
    receiver.expire(ClientInstant(10), &|_| false).unwrap();
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
    assert_eq!(receiver.dropped(), 0);
}
