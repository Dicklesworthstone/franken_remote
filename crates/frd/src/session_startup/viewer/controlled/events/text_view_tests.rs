//! Queued commits keep the complete view at capture, including while blocked.
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

fn capture(receiver: &Receiver, text: &str) {
    let mut queue = receiver.queue.lock().unwrap();
    let original = queue.context;
    queue.events.push_back(Captured {
        event: Event::Text(CommittedText::new(text).unwrap()),
        sampled: ClientInstant(7),
        text_context: Some(original),
        text_until: None,
    });
}

fn replacements() -> [InputView; 4] {
    let original = view();
    [
        InputView {
            geometry: original.geometry.next().unwrap(),
            ..original
        },
        InputView {
            viewport: original.viewport.next().unwrap(),
            ..original
        },
        InputView {
            configuration: original.configuration.next().unwrap(),
            ..original
        },
        InputView {
            recovery: original.recovery.next().unwrap(),
            ..original
        },
    ]
}

#[test]
fn every_replacement_drops_old_text_without_rebinding_or_replay() {
    for replacement in replacements() {
        let mut receiver = receiver();
        capture(&receiver, "old 漢🙂");
        assert!(
            receiver
                .front(replacement, ClientInstant(8))
                .unwrap()
                .is_none()
        );
        assert_eq!(receiver.suspended(), 1);
        assert!(receiver.front(view(), ClientInstant(8)).unwrap().is_none());
        assert_eq!(receiver.suspended(), 1);
    }
}

#[test]
fn backpressure_does_not_rebind_a_pending_commit() {
    let mut receiver = receiver();
    capture(&receiver, "held é");
    assert!(receiver.front(view(), ClientInstant(8)).unwrap().is_some());
    let replacement = replacements()[3];
    assert!(
        receiver
            .front(replacement, ClientInstant(8))
            .unwrap()
            .is_none()
    );
    assert!(receiver.pending.is_none());
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn fresh_capture_uses_published_view_and_keeps_original_time_and_bytes() {
    let mut receiver = receiver();
    let replacement = replacements()[1];
    assert!(
        receiver
            .front(replacement, ClientInstant(8))
            .unwrap()
            .is_none()
    );
    capture(&receiver, "e\u{301}é漢🙂");
    let captured = receiver.front(replacement, ClientInstant(8)).unwrap().unwrap();
    assert_eq!(captured.sampled, ClientInstant(7));
    assert_eq!(captured.text_context.unwrap().view, replacement);
    assert!(matches!(
        &captured.event,
        Event::Text(text) if text.as_str() == "e\u{301}é漢🙂"
    ));
    assert_eq!(receiver.suspended(), 0);
}

#[test]
fn pending_text_is_fenced_even_while_a_producer_holds_the_queue() {
    let mut receiver = receiver();
    capture(&receiver, "original");
    assert!(receiver.front(view(), ClientInstant(8)).unwrap().is_some());
    let shared = receiver.queue.clone();
    let _guard = shared.lock().unwrap();
    assert!(
        receiver
            .front(replacements()[0], ClientInstant(8))
            .unwrap()
            .is_none()
    );
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn replacement_does_not_discard_a_preceding_key_release() {
    let mut receiver = receiver();
    receiver.queue.lock().unwrap().events.push_back(Captured {
        event: Event::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Release,
        },
        sampled: ClientInstant(7),
        text_context: None,
        text_until: None,
    });
    capture(&receiver, "old");
    let replacement = replacements()[2];
    assert!(matches!(
        &receiver
            .front(replacement, ClientInstant(8))
            .unwrap()
            .unwrap()
            .event,
        Event::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));
    receiver.pending = None;
    assert!(
        receiver
            .front(replacement, ClientInstant(8))
            .unwrap()
            .is_none()
    );
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn completed_commit_expires_at_its_original_bound_despite_same_context_renewal() {
    let mut receiver = receiver();
    capture(&receiver, "before renewal");
    receiver
        .queue
        .lock()
        .unwrap()
        .events
        .front_mut()
        .unwrap()
        .text_until = Some(ClientInstant(9));
    assert!(receiver.front(view(), ClientInstant(8)).unwrap().is_some());
    receiver.publish_context(view(), 0, false, true).unwrap();
    assert!(receiver.front(view(), ClientInstant(9)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn later_commit_cannot_refresh_an_earlier_queued_completion_deadline() {
    let mut receiver = receiver();
    {
        let mut queue = receiver.queue.lock().unwrap();
        let context = queue.context;
        for (value, until) in [("expired", 9), ("later", 20)] {
            queue
                .push_bounded(
                    Event::Text(CommittedText::new(value).unwrap()),
                    ClientInstant(8),
                    Some(context),
                    Some(ClientInstant(until)),
                )
                .unwrap();
        }
    }
    assert!(receiver.front(view(), ClientInstant(9)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
    let captured = receiver.front(view(), ClientInstant(9)).unwrap().unwrap();
    assert_eq!(captured.text_until, Some(ClientInstant(20)));
    assert!(matches!(
        &captured.event,
        Event::Text(text) if text.as_str() == "later"
    ));
}
