//! Committed text uses the same production UDP/TLS input and result owners.
//! Native effects are counted by the existing sink; this is not an IME probe.
use super::*;
use events::{CommittedText, TextError};

async fn finish_actions(state: &mut Fixture, client: &Cx, host: &Cx, sequence: u64) {
    let (mut counter, mut ticket) = (10000, 20000);
    let until = now(client).unwrap() + 500_000;
    loop {
        assert!(now(client).unwrap() < until, "text handoff stalled");
        let (left, right) = turn(state, client, host, &mut counter, &mut ticket).await;
        left.unwrap();
        right.unwrap();
        if state.viewer.last_result().is_some_and(
            |event| matches!(event, ResultEvent::Completed(result) if result.sequence == sequence),
        ) {
            break;
        }
    }
}
fn key_operations() -> [Op; 2] {
    [KeyTransition::Press, KeyTransition::Release].map(|transition| Op::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    })
}
#[test]
fn committed_unicode_preserves_original_bytes_and_order_between_physical_actions() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture_with_caps(
            &client,
            &host,
            caps().with(Capability::Text),
        ))
        .await;
        let mut source = state.viewer.capture_input().unwrap();
        assert!(source.supports_text());
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let original = "e\u{301}é漢🙂";
                let mut platform = original.to_owned();
                let at = source.clock().unwrap();
                source.push(physical(KeyTransition::Press), at).unwrap();
                source.push(physical(KeyTransition::Release), at).unwrap();
                source.commit_text(&platform, at).unwrap();
                platform.clear();
                platform.push_str("next composition");
                source.push(physical(KeyTransition::Press), at).unwrap();
                source.push(physical(KeyTransition::Release), at).unwrap();
                finish_actions(&mut state, &client, &host, 4).await;
                let mut expected = key_operations().to_vec();
                expected.extend(original.chars().map(Op::Text));
                expected.extend(key_operations());
                assert_eq!(state.effects.lock().unwrap().operations, expected);
                source.stop();
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn unsupported_text_refuses_before_admission_and_keeps_physical_keys_usable() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        assert!(!source.supports_text());
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let at = source.clock().unwrap();
                assert_eq!(
                    source.commit_text("漢🙂", at),
                    Err(events::Error::UnsupportedText)
                );
                assert_eq!(
                    source.push(Event::Text(CommittedText::new("é").unwrap()), at),
                    Err(events::Error::UnsupportedText)
                );
                assert!(!state.viewer.is_closed());
                assert_eq!(state.viewer.pending_actions(), 0);
                source.push(physical(KeyTransition::Press), at).unwrap();
                source.push(physical(KeyTransition::Release), at).unwrap();
                finish_actions(&mut state, &client, &host, 1).await;
                assert_eq!(state.effects.lock().unwrap().operations, key_operations());
                source.stop();
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
    });
}
#[test]
fn invalid_commits_are_refused_whole_without_a_wire_identity_or_effect() {
    for (value, reason) in [
        (String::new(), TextError::Empty),
        ("a".repeat(MAX_COMMITTED_TEXT_BYTES + 1), TextError::TooLong),
    ] {
        run(move |client, host| async move {
            let mut state = Box::pin(fixture_with_caps(
                &client,
                &host,
                caps().with(Capability::Text),
            ))
            .await;
            let mut source = state.viewer.capture_input().unwrap();
            let at = source.clock().unwrap();
            assert_eq!(
                source.commit_text(&value, at),
                Err(events::Error::Text(reason))
            );
            assert!(state.viewer.is_closed());
            assert!(!state.viewer.pending_send());
            assert_eq!(state.viewer.pending_actions(), 0);
            assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
            state.viewer.close();
            state.host.close();
        });
    }
}
#[test]
fn pending_text_keeps_its_original_deadline_and_is_not_replayed_after_stop() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture_with_caps(
            &client,
            &host,
            caps().with(Capability::Text),
        ))
        .await;
        let mut source = state.viewer.capture_input().unwrap();
        let sampled = source.clock().unwrap();
        source.commit_text("original 漢🙂", sampled).unwrap();
        state.viewer.dispatch_captured().unwrap();
        assert_eq!(state.viewer.pending_actions(), 1);
        let pending = state.viewer.pending.as_ref().unwrap();
        let until = pending.until;
        let bytes = pending.bytes[..pending.len].to_vec();
        assert!(until <= sampled.0 + events::MAX_EVENT_AGE_US);
        source.commit_text("deferred", sampled).unwrap();
        state.viewer.dispatch_captured().unwrap();
        let pending = state.viewer.pending.as_ref().unwrap();
        assert_eq!(pending.until, until);
        assert_eq!(&pending.bytes[..pending.len], bytes);
        source.stop();
        assert!(
            state
                .viewer
                .drive(Duration::ZERO, |_| {}, block)
                .await
                .is_err()
        );
        assert!(state.viewer.events.is_none());
        assert!(!state.viewer.pending_send());
        assert_eq!(
            source.commit_text("late", sampled),
            Err(events::Error::Closed)
        );
        assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
        state.host.close();
    });
}
#[test]
fn maximum_sized_text_still_obeys_event_capacity_and_terminal_overflow() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture_with_caps(
            &client,
            &host,
            caps().with(Capability::Text),
        ))
        .await;
        let mut source = state.viewer.capture_input().unwrap();
        let value = "🙂".repeat(MAX_COMMITTED_TEXT_BYTES / 4);
        let at = source.clock().unwrap();
        for _ in 0..events::MAX_EVENTS {
            source.commit_text(&value, at).unwrap();
        }
        assert_eq!(
            source.commit_text("overflow", at),
            Err(events::Error::Overflow)
        );
        assert!(state.viewer.is_closed());
        assert_eq!(state.viewer.pending_actions(), 0);
        state.viewer.close();
        assert!(state.viewer.events.is_none());
        assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
        state.host.close();
    });
}
