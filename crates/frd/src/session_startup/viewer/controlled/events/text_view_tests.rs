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
    Receiver::new(Arc::new(Mutex::new(Queue {
        events: VecDeque::new(),
        last_sample: ClientInstant(7),
        view: view(),
    })))
}

fn capture(receiver: &Receiver, text: &str) {
    let mut queue = receiver.queue.lock().unwrap();
    let original = queue.view;
    queue.events.push_back(Captured {
        event: Event::Text(CommittedText::new(text).unwrap()),
        sampled: ClientInstant(7),
        text_view: Some(original),
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
        assert!(receiver.front(replacement).unwrap().is_none());
        assert_eq!(receiver.suspended(), 1);
        assert!(receiver.front(view()).unwrap().is_none());
        assert_eq!(receiver.suspended(), 1);
    }
}

#[test]
fn backpressure_does_not_rebind_a_pending_commit() {
    let mut receiver = receiver();
    capture(&receiver, "held é");
    assert!(receiver.front(view()).unwrap().is_some());
    let replacement = replacements()[3];
    assert!(receiver.front(replacement).unwrap().is_none());
    assert!(receiver.pending.is_none());
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn fresh_capture_uses_published_view_and_keeps_original_time_and_bytes() {
    let mut receiver = receiver();
    let replacement = replacements()[1];
    assert!(receiver.front(replacement).unwrap().is_none());
    capture(&receiver, "e\u{301}é漢🙂");
    let captured = receiver.front(replacement).unwrap().unwrap();
    assert_eq!(captured.sampled, ClientInstant(7));
    assert_eq!(captured.text_view, Some(replacement));
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
    assert!(receiver.front(view()).unwrap().is_some());
    let shared = receiver.queue.clone();
    let _guard = shared.lock().unwrap();
    assert!(receiver.front(replacements()[0]).unwrap().is_none());
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
        text_view: None,
    });
    capture(&receiver, "old");
    let replacement = replacements()[2];
    assert!(matches!(
        &receiver.front(replacement).unwrap().unwrap().event,
        Event::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));
    receiver.pending = None;
    assert!(receiver.front(replacement).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
}
