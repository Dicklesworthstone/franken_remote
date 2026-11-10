//! Local drafts and queued commits cannot cross an actual input suspension.
//! A compositor candidate gap only disables Send; it preserves a valid draft.
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
fn token(receiver: &Receiver) -> TextContext {
    TextContext {
        queue: Arc::downgrade(&receiver.queue),
        state: receiver.queue.lock().unwrap().context,
    }
}
fn text(value: &str) -> Event {
    Event::Text(CommittedText::new(value).unwrap())
}
fn submit(receiver: &Receiver, context: &TextContext, value: &str) -> Result<(), Error> {
    assert!(context.belongs_to(&Arc::downgrade(&receiver.queue)));
    receiver
        .queue
        .lock()
        .unwrap()
        .push(text(value), ClientInstant(8), Some(context.state))
}

#[test]
fn equal_generations_on_another_source_do_not_rebind_a_draft() {
    let first = receiver();
    let second = receiver();
    let original = token(&first);
    let foreign = token(&second);
    assert_eq!(original.state, foreign.state);
    assert!(original != foreign);
    assert!(original == original.clone());
    assert!(original.belongs_to(&Arc::downgrade(&first.queue)));
    assert!(!original.belongs_to(&Arc::downgrade(&second.queue)));
    assert_eq!(Arc::strong_count(&first.queue), 1);
    drop(first);
    assert!(original.queue.upgrade().is_none());
    assert!(original != foreign);
}

#[test]
fn compositor_gap_preserves_draft_but_refuses_send_without_queue_or_clock_effect() {
    let mut receiver = receiver();
    let original = token(&receiver);
    receiver.publish_context(view(), 0, false, false).unwrap();
    assert!(original == token(&receiver));
    assert_eq!(
        receiver.queue.lock().unwrap().context_ready(original.state),
        Ok(false)
    );
    assert_eq!(
        submit(&receiver, &original, "held é"),
        Err(Error::TextNotReady)
    );
    {
        let queue = receiver.queue.lock().unwrap();
        assert!(queue.events.is_empty());
        assert_eq!(queue.last_sample, ClientInstant(7));
    }
    receiver.publish_context(view(), 0, false, true).unwrap();
    assert!(original == token(&receiver));
    submit(&receiver, &original, "e\u{301}é漢🙂").unwrap();
    let captured = receiver.front(view(), ClientInstant(10)).unwrap().unwrap();
    assert_eq!(captured.sampled, ClientInstant(8));
    assert_eq!(captured.text_context, Some(original.state));
    assert!(matches!(
        &captured.event,
        Event::Text(value) if value.as_str() == "e\u{301}é漢🙂"
    ));
    assert_eq!(receiver.suspended(), 0);
}

#[test]
fn suspension_then_same_view_resume_permanently_rejects_original_draft_and_pending_commit() {
    let mut receiver = receiver();
    let original = token(&receiver);
    submit(&receiver, &original, "old").unwrap();
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_some());
    receiver.publish_context(view(), 1, true, false).unwrap();
    let suspended = token(&receiver);
    assert!(suspended.state.suspended);
    assert_eq!(
        submit(&receiver, &original, "old"),
        Err(Error::StaleTextContext)
    );
    receiver.publish_context(view(), 1, false, true).unwrap();
    assert!(original != token(&receiver));
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
    assert_eq!(
        submit(&receiver, &original, "old"),
        Err(Error::StaleTextContext)
    );
    assert_eq!(
        submit(&receiver, &suspended, "stale"),
        Err(Error::StaleTextContext)
    );
    let fresh = token(&receiver);
    submit(&receiver, &fresh, "fresh").unwrap();
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_some());
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn suspension_and_resume_between_publications_still_invalidates_the_same_view() {
    let mut receiver = receiver();
    let original = token(&receiver);
    submit(&receiver, &original, "queued").unwrap();
    // Both observations are ready and name identical generations; only the
    // original InputClient's monotonic lapse history reveals the interruption.
    receiver.publish_context(view(), 1, false, true).unwrap();
    assert!(original != token(&receiver));
    assert_eq!(
        submit(&receiver, &original, "old"),
        Err(Error::StaleTextContext)
    );
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn legacy_commit_captured_while_suspended_cannot_replay_after_same_view_resume() {
    let mut receiver = receiver();
    receiver.publish_context(view(), 1, true, false).unwrap();
    receiver
        .queue
        .lock()
        .unwrap()
        .push(text("already committed"), ClientInstant(8), None)
        .unwrap();
    receiver.publish_context(view(), 1, false, true).unwrap();
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
}

#[test]
fn contended_publication_cannot_revive_old_pending_or_queued_text() {
    let mut receiver = receiver();
    let original = token(&receiver);
    submit(&receiver, &original, "pending").unwrap();
    submit(&receiver, &original, "queued").unwrap();
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_some());
    let shared = receiver.queue.clone();
    let mut guard = shared.lock().unwrap();
    receiver.publish_context(view(), 1, true, false).unwrap();
    receiver.publish_context(view(), 1, false, true).unwrap();
    assert_eq!(guard.context, original.state);
    // A producer racing both failed publications can still capture the old
    // snapshot, but the dispatch owner's retained revision rejects it too.
    guard
        .push(text("racing"), ClientInstant(9), Some(original.state))
        .unwrap();
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
    drop(guard);
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 2);
    assert!(receiver.front(view(), ClientInstant(10)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 3);
    assert!(original != token(&receiver));
    assert_eq!(
        submit(&receiver, &original, "old"),
        Err(Error::StaleTextContext)
    );
}

#[test]
fn repeated_publications_do_not_churn_context_and_exhaustion_never_wraps() {
    let mut state = TextState::new(view(), 2, false).unwrap();
    let original = state;
    state.update(view(), 2, false).unwrap();
    assert_eq!(state, original);
    state.update(view(), 3, true).unwrap();
    let suspended = state;
    state.update(view(), 3, true).unwrap();
    assert_eq!(state, suspended);
    assert_eq!(state.update(view(), 2, false), Err(Error::Unavailable));
    assert_eq!(state, suspended);
    assert_eq!(
        state.update(view(), u32::MAX, false),
        Err(Error::Unavailable)
    );
    assert_eq!(state, suspended);
    assert_eq!(
        TextState::new(view(), u32::MAX, false),
        Err(Error::Unavailable)
    );
    state.revision = u64::MAX;
    let exhausted = state;
    assert_eq!(state.update(view(), 3, false), Err(Error::Unavailable));
    assert_eq!(state, exhausted);
}

#[test]
fn whole_context_refusal_preserves_earlier_physical_release_and_time() {
    let mut receiver = receiver();
    let original = token(&receiver);
    receiver
        .queue
        .lock()
        .unwrap()
        .push(
            Event::Key {
                key: PhysicalKey::new(4).unwrap(),
                transition: KeyTransition::Release,
            },
            ClientInstant(8),
            None,
        )
        .unwrap();
    receiver.publish_context(view(), 1, false, true).unwrap();
    {
        let mut queue = receiver.queue.lock().unwrap();
        assert_eq!(
            queue.push(text("refused"), ClientInstant(9), Some(original.state)),
            Err(Error::StaleTextContext)
        );
        assert_eq!(queue.last_sample, ClientInstant(8));
        assert_eq!(queue.events.len(), 1);
    }
    assert!(matches!(
        &receiver
            .front(view(), ClientInstant(10))
            .unwrap()
            .unwrap()
            .event,
        Event::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));
    assert_eq!(receiver.suspended(), 0);
}

#[test]
fn bounded_completion_cannot_inherit_readiness_after_a_compositor_gap() {
    let mut receiver = receiver();
    let original = token(&receiver);
    receiver.publish_context(view(), 0, false, false).unwrap();
    {
        let mut queue = receiver.queue.lock().unwrap();
        assert_eq!(
            queue.push_bounded(
                text("completed"),
                ClientInstant(8),
                Some(original.state),
                Some(ClientInstant(9)),
            ),
            Err(Error::TextNotReady)
        );
        assert!(queue.events.is_empty());
        assert_eq!(queue.last_sample, ClientInstant(7));
    }
    receiver.publish_context(view(), 0, false, true).unwrap();
    assert!(original == token(&receiver));
    receiver
        .queue
        .lock()
        .unwrap()
        .push_bounded(
            text("original deadline"),
            ClientInstant(8),
            Some(original.state),
            Some(ClientInstant(9)),
        )
        .unwrap();
    assert!(receiver.front(view(), ClientInstant(9)).unwrap().is_none());
    assert_eq!(receiver.suspended(), 1);
}
