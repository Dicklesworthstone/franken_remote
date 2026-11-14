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

fn capture_in_original_context(receiver: &Receiver) -> InputContext {
    let mut queue = receiver.queue.lock().unwrap();
    let context = InputContext(TextContext {
        queue: Arc::downgrade(&receiver.queue),
        state: queue.context,
    });
    for event in [
        key(0xE1, KeyTransition::Press),
        key(0x04, KeyTransition::Press),
    ] {
        queue
            .push_bounded(
                event,
                ClientInstant(7),
                Some(context.0.state),
                Some(ClientInstant(20)),
            )
            .unwrap();
    }
    context
}

#[test]
fn native_keys_retain_every_original_view_generation_through_queued_and_pending_dispatch() {
    let old = view();
    for replacement in [
        InputView {
            geometry: old.geometry.next().unwrap(),
            ..old
        },
        InputView {
            viewport: old.viewport.next().unwrap(),
            ..old
        },
        InputView {
            configuration: old.configuration.next().unwrap(),
            ..old
        },
        InputView {
            recovery: old.recovery.next().unwrap(),
            ..old
        },
    ] {
        for pending in [false, true] {
            let mut receiver = receiver();
            let original = capture_in_original_context(&receiver);
            if pending {
                let first = receiver.front(old, ClientInstant(8)).unwrap().unwrap();
                assert_eq!(first.sampled, ClientInstant(7));
                assert_eq!(first.send_until, Some(ClientInstant(20)));
                assert_eq!(first.text_context, Some(original.0.state));
            }
            assert!(matches!(
                receiver.front(replacement, ClientInstant(9)),
                Err(Error::StaleInputContext)
            ));
            // Returning to numerically equal generations never revives the
            // original modifier or permits its dependent letter to execute.
            assert!(matches!(
                receiver.front(old, ClientInstant(10)),
                Err(Error::StaleInputContext)
            ));
            assert_eq!(receiver.suspended(), 0, "terminal fence, not a skipped key");
        }
    }
}

#[test]
fn native_keys_cannot_outlive_a_suspend_resume_that_happens_between_ui_polls() {
    let mut receiver = receiver();
    capture_in_original_context(&receiver);
    receiver.publish_context(view(), 1, false, true).unwrap();
    assert!(matches!(
        receiver.front(view(), ClientInstant(9)),
        Err(Error::StaleInputContext)
    ));
}

#[test]
fn compositor_wait_never_refreshes_the_native_callback_deadline() {
    let mut receiver = receiver();
    let original = capture_in_original_context(&receiver);
    receiver.publish_context(view(), 0, false, false).unwrap();
    assert_eq!(
        receiver
            .queue
            .lock()
            .unwrap()
            .context_ready(original.0.state),
        Ok(false)
    );
    receiver.publish_context(view(), 0, false, true).unwrap();
    assert_eq!(
        receiver
            .queue
            .lock()
            .unwrap()
            .context_ready(original.0.state),
        Ok(true)
    );
    receiver.expire(ClientInstant(19), &|_| false).unwrap();
    assert_eq!(
        receiver.expire(ClientInstant(20), &|_| false),
        Err(Error::Expired)
    );
    assert_eq!(
        receiver.expire(ClientInstant(21), &|_| false),
        Err(Error::Expired)
    );
}

#[test]
fn input_contexts_from_other_producers_with_equal_generations_do_not_match() {
    let original = receiver();
    let replacement = receiver();
    let first = capture_in_original_context(&original);
    let second = capture_in_original_context(&replacement);
    assert!(first != second);
    assert!(first.0.belongs_to(&Arc::downgrade(&original.queue)));
    assert!(!first.0.belongs_to(&Arc::downgrade(&replacement.queue)));
}

#[test]
fn newer_input_cannot_be_overtaken_by_a_still_fresh_delayed_callback() {
    let receiver = receiver();
    let original = capture_in_original_context(&receiver);
    let mut queue = receiver.queue.lock().unwrap();
    queue
        .push(key(0x05, KeyTransition::Press), ClientInstant(9), None)
        .unwrap();
    let before = queue.events.len();
    // Capacity alone does not authorize the old suffix after a newer action.
    // Source treats this clock refusal as terminal, never restamping it.
    assert_eq!(
        queue.push_bounded(
            key(0x06, KeyTransition::Press),
            ClientInstant(7),
            Some(original.0.state),
            Some(ClientInstant(20)),
        ),
        Err(Error::Clock)
    );
    assert_eq!(queue.events.len(), before, "no old-key prefix admitted");
    assert_eq!(queue.last_sample, ClientInstant(9));
}
