//! Production UDP/TLS, original input receipts and counted OS submissions.
//! These tests qualify the handoff, not a physical renderer or native window.
use super::super::events::{self, Event};
use super::*;
use fr_client::input::viewport::{LocalPoint, PositionedAction, SurfaceRect};

fn physical(transition: KeyTransition) -> Event {
    Event::Key {
        key: PhysicalKey::new(4).unwrap(),
        transition,
    }
}
fn layout(state: &mut Fixture) -> fr_client::input::viewport::Layout {
    let layout = state
        .viewer
        .configure_viewport(bounds(), SurfaceRect::new(0, 0, 640, 480).unwrap())
        .unwrap();
    state.viewer.confirm_viewport(&layout).unwrap();
    layout
}
fn assert_captured_operations(operations: &[Op]) {
    let expected = vec![
        Op::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Press,
        },
        Op::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Repeat,
        },
        Op::Key {
            key: PhysicalKey::new(4).unwrap(),
            transition: KeyTransition::Release,
        },
        Op::Absolute(DesktopPoint { x: 100, y: 50 }),
        Op::Absolute(DesktopPoint { x: 110, y: 60 }),
        Op::Button {
            button: PointerButton::Primary,
            pressed: true,
        },
        Op::Absolute(DesktopPoint { x: 110, y: 60 }),
        Op::Button {
            button: PointerButton::Primary,
            pressed: false,
        },
        Op::Absolute(DesktopPoint { x: 110, y: 60 }),
        Op::Scroll {
            x: 0,
            y: -1,
            unit: ScrollUnit::Lines,
        },
    ];
    // The pointer-only motion travels as a replaceable datagram that the host
    // tolerates missing (fr-client `InputClient::pointer`); every key, button
    // and positioned move is reliable and must arrive exactly, in order.
    let mut without_motion = expected.clone();
    without_motion.remove(3);
    assert!(
        operations == expected.as_slice() || operations == without_motion.as_slice(),
        "{operations:?}"
    );
}
#[test]
fn captured_events_reach_native_session_in_order_with_motion_coalescing() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture_with_caps(
            &client,
            &host,
            caps()
                .with(Capability::Buttons)
                .with(Capability::LineScroll)
                .with(Capability::Repeat),
        ))
        .await;
        let layout = layout(&mut state);
        let mut source = state.viewer.capture_input().unwrap();
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let at = source.clock().unwrap();
                source.push(physical(KeyTransition::Press), at).unwrap();
                source.push(physical(KeyTransition::Repeat), at).unwrap();
                source.push(physical(KeyTransition::Release), at).unwrap();
                for x in 1..=200 {
                    source
                        .push(Event::Pointer(layout.at(LocalPoint::pixels(x, 100))), at)
                        .unwrap();
                }
                for pressed in [true, false] {
                    source
                        .push(
                            Event::Positioned {
                                location: layout.at(LocalPoint::pixels(220, 120)),
                                action: PositionedAction::Button {
                                    button: PointerButton::Primary,
                                    pressed,
                                },
                            },
                            at,
                        )
                        .unwrap();
                }
                source
                    .push(
                        Event::Positioned {
                            location: layout.at(LocalPoint::pixels(220, 120)),
                            action: PositionedAction::Scroll {
                                x: 0,
                                y: -1,
                                unit: ScrollUnit::Lines,
                            },
                        },
                        at,
                    )
                    .unwrap();
                let (mut counter, mut ticket) = (10000, 20000);
                let until = now(&client).unwrap() + 500_000;
                loop {
                    assert!(now(&client).unwrap() < until, "event handoff stalled");
                    let (left, right) =
                        turn(&mut state, &client, &host, &mut counter, &mut ticket).await;
                    left.unwrap();
                    right.unwrap();
                    if state.viewer.last_result().is_some_and(|event| {
                        matches!(event,
                    ResultEvent::Completed(result) if result.sequence == 5)
                    }) {
                        break;
                    }
                }
                let operations = state.effects.lock().unwrap().operations.clone();
                assert_captured_operations(&operations);
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
fn focus_loss_cancels_an_unpolled_drive_and_never_replays_queued_keys() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let at = source.clock().unwrap();
        source.push(physical(KeyTransition::Press), at).unwrap();
        let operation = state.viewer.drive(Duration::from_millis(1), |_| {}, block);
        source.stop();
        assert!(Box::pin(operation).await.is_err());
        assert!(state.viewer.is_closed());
        assert_eq!(
            source.push(physical(KeyTransition::Release), at),
            Err(events::Error::Closed)
        );
        assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
        state.host.close();
    });
}
#[test]
fn dropping_event_source_fences_control_without_polling_the_network() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let source = state.viewer.capture_input().unwrap();
        assert!(matches!(
            state.viewer.capture_input(),
            Err(Error::Capture(events::Error::AlreadyAttached))
        ));
        drop(source);
        assert!(state.viewer.is_closed());
        assert_eq!(state.viewer.action(key(true)), Err(Error::Closed));
        state.host.close();
    });
}
#[test]
fn a_producer_can_see_its_queued_events_and_pace_below_capacity() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let at = source.clock().unwrap();
        assert_eq!(source.queued(), Some(0));
        source.push(physical(KeyTransition::Press), at).unwrap();
        source.push(physical(KeyTransition::Release), at).unwrap();
        assert_eq!(source.queued(), Some(2));
        // A paced producer stops before the terminal bound.
        while source.queued().is_some_and(|n| n < events::MAX_EVENTS) {
            source.push(physical(KeyTransition::Press), at).unwrap();
        }
        assert_eq!(source.queued(), Some(events::MAX_EVENTS));
        assert!(!state.viewer.is_closed());
        state.host.close();
    });
}
#[test]
fn captured_event_capacity_is_terminal_not_a_dropped_release() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let at = source.clock().unwrap();
        for _ in 0..events::MAX_EVENTS {
            source.push(physical(KeyTransition::Press), at).unwrap();
        }
        assert_eq!(
            source.push(physical(KeyTransition::Release), at),
            Err(events::Error::Overflow)
        );
        assert!(state.viewer.is_closed());
        state.host.close();
    });
}
#[test]
fn captured_event_age_survives_encoding_before_transport_backpressure() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let sampled = source.clock().unwrap();
        source
            .push(physical(KeyTransition::Press), sampled)
            .unwrap();
        state.viewer.dispatch_captured().unwrap();
        assert!(state.viewer.pending_send());
        assert!(
            state.viewer.pending.as_ref().unwrap().until <= sampled.0 + events::MAX_EVENT_AGE_US
        );
        source.stop();
        assert!(
            state
                .viewer
                .drive(Duration::ZERO, |_| {}, block)
                .await
                .is_err()
        );
        assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
        state.host.close();
    });
}
#[test]
fn original_native_deadline_survives_source_queue_and_transport_encoding() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let sampled = source.clock().unwrap();
        let until = ClientInstant(sampled.0 + events::MAX_EVENT_AGE_US / 2);
        source
            .push_until(physical(KeyTransition::Press), sampled, until)
            .unwrap();
        state.viewer.dispatch_captured().unwrap();
        assert!(state.viewer.pending_send());
        assert!(
            state.viewer.pending.as_ref().unwrap().until <= until.0,
            "encoding must not give a deferred native key another age window"
        );
        source.stop();
        assert!(
            state
                .viewer
                .drive(Duration::ZERO, |_| {}, block)
                .await
                .is_err()
        );
        assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
        state.host.close();
    });
}

#[test]
fn an_already_expired_native_key_fences_the_original_source_before_enqueue() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let sampled = source.clock().unwrap();
        assert_eq!(
            source.push_until(physical(KeyTransition::Press), sampled, sampled),
            Err(events::Error::Expired)
        );
        assert!(state.viewer.is_closed());
        assert_eq!(
            source.push(physical(KeyTransition::Press), sampled),
            Err(events::Error::Closed)
        );
        assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
        state.host.close();
    });
}
#[test]
fn captured_coordinates_cannot_be_reinterpreted_under_a_new_layout() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let old = layout(&mut state);
        let mut source = state.viewer.capture_input().unwrap();
        source
            .push(
                Event::Pointer(old.at(LocalPoint::pixels(20, 20))),
                source.clock().unwrap(),
            )
            .unwrap();
        let _replacement = layout(&mut state);
        assert!(matches!(
            state.viewer.drive(Duration::ZERO, |_| {}, block).await,
            Err(Error::Viewport(fr_client::input::viewport::Error::Obsolete))
        ));
        assert!(state.viewer.is_closed());
        state.host.close();
    });
}
#[test]
fn future_sample_cannot_mint_a_longer_native_event_lifetime() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let at = ClientInstant(source.clock().unwrap().0 + 1_000_000);
        assert_eq!(
            source.push(physical(KeyTransition::Press), at),
            Err(events::Error::Clock)
        );
        assert!(state.viewer.is_closed());
        state.host.close();
    });
}

/// Drive both sessions until `done` holds for the host's submitted operations.
async fn turns_until(
    state: &mut Fixture,
    client: &Cx,
    host: &Cx,
    done: impl Fn(&[Op]) -> bool,
) -> Vec<Op> {
    let (mut counter, mut ticket) = (30000, 40000);
    let until = now(client).unwrap() + 1_000_000;
    loop {
        let operations = state.effects.lock().unwrap().operations.clone();
        if done(&operations) {
            return operations;
        }
        assert!(now(client).unwrap() < until, "stalled at {operations:?}");
        let (left, right) = turn(state, client, host, &mut counter, &mut ticket).await;
        left.unwrap();
        right.unwrap();
    }
}
/// Hold local dispatch (no turn) until events sampled now are past their bound.
async fn age_past_dispatch_bound(client: &Cx) {
    asupersync::time::sleep(
        client.now(),
        Duration::from_micros(events::MAX_EVENT_AGE_US + 20_000),
    )
    .await;
}

#[test]
fn an_aged_release_of_a_held_key_is_still_delivered() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let mut source = state.viewer.capture_input().unwrap();
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let at = source.clock().unwrap();
                source.push(physical(KeyTransition::Press), at).unwrap();
                Box::pin(turns_until(&mut state, &client, &host, |ops| {
                    ops.len() == 1
                }))
                .await;
                // The release waits past its bound (a view awaiting its
                // visibility callback holds dispatch the same way). Leaving the
                // key held on the host would be worse than releasing it late.
                let released = source.clock().unwrap();
                age_past_dispatch_bound(&client).await;
                source
                    .push(physical(KeyTransition::Release), released)
                    .unwrap();
                let operations = Box::pin(turns_until(&mut state, &client, &host, |ops| {
                    ops.len() == 2
                }))
                .await;
                assert_eq!(
                    operations,
                    [
                        Op::Key {
                            key: PhysicalKey::new(4).unwrap(),
                            transition: KeyTransition::Press
                        },
                        Op::Key {
                            key: PhysicalKey::new(4).unwrap(),
                            transition: KeyTransition::Release
                        },
                    ]
                );
                assert!(!state.viewer.is_closed());
                assert_eq!(state.viewer.input_dropped_aged(), 0);
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
fn an_aged_press_is_dropped_with_its_release_and_control_continues() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture(&client, &host)).await;
        let layout = layout(&mut state);
        let mut source = state.viewer.capture_input().unwrap();
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let at = source.clock().unwrap();
                source.push(physical(KeyTransition::Press), at).unwrap();
                source
                    .push(Event::Pointer(layout.at(LocalPoint::pixels(20, 20))), at)
                    .unwrap();
                age_past_dispatch_bound(&client).await;
                // Obsolete local input: dropped and counted, never sent late,
                // and never a reason to end control.
                let (mut counter, mut ticket) = (30000, 40000);
                let mut quiet_turns = async |state: &mut Fixture| {
                    let quiet = now(&client).unwrap() + 150_000;
                    while now(&client).unwrap() < quiet {
                        let (left, right) =
                            turn(state, &client, &host, &mut counter, &mut ticket).await;
                        left.unwrap();
                        right.unwrap();
                    }
                };
                quiet_turns(&mut state).await;
                assert_eq!(state.viewer.input_dropped_aged(), 2);
                // A FRESH release of that dropped press goes with it: the host
                // never saw the press, so sending it would be a protocol error.
                let fresh = source.clock().unwrap();
                source
                    .push(physical(KeyTransition::Release), fresh)
                    .unwrap();
                quiet_turns(&mut state).await;
                assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
                assert_eq!(state.viewer.input_dropped_aged(), 3);
                assert!(!state.viewer.is_closed());
                // Fresh input on the same grant still reaches the host.
                let fresh = source.clock().unwrap();
                source.push(physical(KeyTransition::Press), fresh).unwrap();
                source
                    .push(physical(KeyTransition::Release), fresh)
                    .unwrap();
                let operations = Box::pin(turns_until(&mut state, &client, &host, |ops| {
                    ops.len() == 2
                }))
                .await;
                assert_eq!(
                    operations[0],
                    Op::Key {
                        key: PhysicalKey::new(4).unwrap(),
                        transition: KeyTransition::Press
                    }
                );
                assert_eq!(state.viewer.input_dropped_aged(), 3);
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
/// A key or button pressed on a stale view is refused (plan 11.3). When the
/// view recovers before the user lets go, the host never saw the press, so
/// its release goes with it: sending it would be a protocol error that ended
/// control on an ordinary keystroke under load.
#[test]
fn a_press_refused_by_a_suspension_takes_its_release_after_input_resumes() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture_with_caps(
            &client,
            &host,
            caps().with(Capability::Buttons),
        ))
        .await;
        let layout = layout(&mut state);
        let mut source = state.viewer.capture_input().unwrap();
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let click = |pressed| Event::Positioned {
                    location: layout.at(LocalPoint::pixels(20, 20)),
                    action: PositionedAction::Button {
                        button: PointerButton::Primary,
                        pressed,
                    },
                };
                // No host turns: no fresh evidence, so the view lapses.
                let until = state
                    .viewer
                    .input
                    .view_deadline(ClientInstant(now(&client).unwrap()))
                    .unwrap()
                    .0;
                while now(&client).unwrap() < until {
                    asupersync::time::sleep(client.now(), Duration::from_millis(1)).await;
                }
                state
                    .viewer
                    .drive(Duration::ZERO, |_| {}, block)
                    .await
                    .unwrap();
                assert!(state.viewer.input.suspended_since().is_some());
                let at = source.clock().unwrap();
                source.push(physical(KeyTransition::Press), at).unwrap();
                source.push(click(true), at).unwrap();
                for _ in 0..4 {
                    state
                        .viewer
                        .drive(Duration::ZERO, |_| {}, block)
                        .await
                        .unwrap();
                }
                assert_eq!(state.viewer.input_dropped_suspended(), 2);
                // Fresh evidence and a ticket issued after the lapse resume
                // input on the same grant.
                let (mut counter, mut ticket) = (30000, 40000);
                let resume_by = now(&client).unwrap() + 3_000_000;
                while state.viewer.input.suspended_since().is_some() {
                    assert!(now(&client).unwrap() < resume_by, "input never resumed");
                    let (left, right) =
                        turn(&mut state, &client, &host, &mut counter, &mut ticket).await;
                    left.unwrap();
                    right.unwrap();
                }
                // The user lets go only now.
                let fresh = source.clock().unwrap();
                source
                    .push(physical(KeyTransition::Release), fresh)
                    .unwrap();
                source.push(click(false), fresh).unwrap();
                let quiet = now(&client).unwrap() + 150_000;
                while now(&client).unwrap() < quiet {
                    let (left, right) =
                        turn(&mut state, &client, &host, &mut counter, &mut ticket).await;
                    left.unwrap();
                    right.unwrap();
                }
                assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
                assert!(!state.viewer.is_closed());
                assert_eq!(state.viewer.input_dropped_suspended(), 4);
                assert_eq!(state.viewer.input_dropped_aged(), 0);
                // Fresh input on the same grant still reaches the host.
                let fresh = source.clock().unwrap();
                source.push(physical(KeyTransition::Press), fresh).unwrap();
                source
                    .push(physical(KeyTransition::Release), fresh)
                    .unwrap();
                let operations = Box::pin(turns_until(&mut state, &client, &host, |ops| {
                    ops.len() == 2
                }))
                .await;
                let op = |transition| Op::Key {
                    key: PhysicalKey::new(4).unwrap(),
                    transition,
                };
                assert_eq!(
                    operations,
                    [op(KeyTransition::Press), op(KeyTransition::Release)]
                );
                assert_eq!(state.viewer.input_dropped_suspended(), 4);
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

mod text;

#[test]
fn native_supervisor_gets_only_original_stop_and_granted_capabilities() {
    run(|client, host| async move {
        let mut state = Box::pin(fixture_with_caps(
            &client,
            &host,
            caps().with(Capability::Repeat),
        ))
        .await;
        let source = state.viewer.capture_input().unwrap();
        assert_eq!(source.capabilities(), caps().with(Capability::Repeat));
        assert!(!source.supports_text());
        let stop = source.control();
        assert!(!stop.is_stopped());
        stop.stop();
        assert!(state.viewer.is_closed());
        assert!(source.clock().is_err());
        assert_eq!(state.viewer.action(key(true)), Err(Error::Closed));
        state.host.close();
    });
}

mod native;
