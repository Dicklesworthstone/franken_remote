//! Actual negotiated UDP/TLS with the production viewer/host and counted sinks.
//! Window layout and frame visibility are explicit fixtures, not a native GUI.
use super::*;
use fr_client::input::viewport::{
    self as mapping, Layout, LocalPoint, PositionedAction, SurfaceRect,
};
use fr_wire::input_result::SequenceSpace;

fn button(pressed: bool) -> PositionedAction {
    PositionedAction::Button {
        button: PointerButton::Primary,
        pressed,
    }
}
async fn mapped_fixture(client: &Cx, host: &Cx) -> Fixture {
    Box::pin(fixture_with_caps(
        client,
        host,
        caps()
            .with(Capability::Buttons)
            .with(Capability::LineScroll),
    ))
    .await
}
fn window(state: &mut Fixture) -> Layout {
    let layout = state
        .viewer
        .configure_viewport(bounds(), SurfaceRect::new(40, 30, 640, 600).unwrap())
        .unwrap();
    assert_eq!(
        layout.destination(),
        SurfaceRect::new(40, 90, 640, 480).unwrap()
    );
    state.viewer.confirm_viewport(&layout).unwrap();
    layout
}
async fn receipt(
    state: &mut Fixture,
    client: &Cx,
    host: &Cx,
    counters: &mut (u128, u128),
    space: SequenceSpace,
    sequence: u64,
) {
    let until = now(client).unwrap() + 800_000;
    loop {
        assert!(now(client).unwrap() < until, "native result not returned");
        let (left, right) = turn(state, client, host, &mut counters.0, &mut counters.1).await;
        left.unwrap();
        right.unwrap();
        if matches!(state.viewer.last_result(),
            Some(ResultEvent::Completed(result) | ResultEvent::Pointer(result))
                if result.space == space && result.sequence == sequence
                    && result.outcome == fr_core::input_sequence::InputOutcome::SubmittedToOs)
        {
            assert!(!state.viewer.pending_send());
            return;
        }
    }
}

async fn scroll_and_release(
    state: &mut Fixture,
    layout: &Layout,
    client: &Cx,
    host: &Cx,
    counters: &mut (u128, u128),
) {
    let scroll = PositionedAction::Scroll {
        x: 0,
        y: -3,
        unit: ScrollUnit::Lines,
    };
    assert_eq!(
        state
            .viewer
            .action_in_view(&layout.at(LocalPoint::pixels(360, 330)), scroll)
            .unwrap()
            .sequence,
        1
    );
    receipt(state, client, host, counters, SequenceSpace::Action, 1).await;
    assert_eq!(
        state
            .viewer
            .action_in_view(&layout.at(LocalPoint::pixels(360, 330)), button(false))
            .unwrap()
            .sequence,
        2
    );
    receipt(state, client, host, counters, SequenceSpace::Action, 2).await;
    assert!(
        state
            .effects
            .lock()
            .unwrap()
            .operations
            .contains(&Op::Scroll {
                x: 0,
                y: -3,
                unit: ScrollUnit::Lines
            })
    );
}

#[test]
fn mapped_window_pointer_click_and_scroll_cross_the_actual_controlled_connection() {
    run(|client, host| async move {
        let mut state = Box::pin(mapped_fixture(&client, &host)).await;
        let layout = window(&mut state);
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let mut counters = (10000, 20000);
                assert_eq!(
                    state
                        .viewer
                        .pointer_in_view(&layout.at(LocalPoint::pixels(200, 210)))
                        .unwrap()
                        .sequence,
                    0
                );
                assert_eq!(
                    state
                        .viewer
                        .action_in_view(&layout.at(LocalPoint::pixels(360, 330)), button(true)),
                    Err(Error::Backpressure)
                );
                receipt(
                    &mut state,
                    &client,
                    &host,
                    &mut counters,
                    SequenceSpace::Pointer,
                    0,
                )
                .await;
                assert_eq!(
                    state.effects.lock().unwrap().operations,
                    [Op::Absolute(DesktopPoint { x: 80, y: 60 })]
                );
                assert_eq!(
                    state
                        .viewer
                        .action_in_view(&layout.at(LocalPoint::pixels(360, 330)), button(true))
                        .unwrap()
                        .sequence,
                    0
                );
                receipt(
                    &mut state,
                    &client,
                    &host,
                    &mut counters,
                    SequenceSpace::Action,
                    0,
                )
                .await;
                assert_eq!(
                    &state.effects.lock().unwrap().operations[1..],
                    [
                        Op::Absolute(DesktopPoint { x: 160, y: 120 }),
                        Op::Button {
                            button: PointerButton::Primary,
                            pressed: true
                        },
                    ]
                );
                scroll_and_release(&mut state, &layout, &client, &host, &mut counters).await;
                assert_eq!(state.viewer.pending_actions(), 0);
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn zoom_preserves_encoded_intent_and_deadline_but_rejects_old_sampled_events() {
    run(|client, host| async move {
        let mut state = Box::pin(mapped_fixture(&client, &host)).await;
        let layout = window(&mut state);
        let sampled = layout.at(LocalPoint::pixels(360, 330));
        assert_eq!(
            state
                .viewer
                .action_in_view(&sampled, button(true))
                .unwrap()
                .sequence,
            0
        );
        let pending = state.viewer.pending.as_ref().unwrap();
        let original = pending.bytes[..pending.len].to_vec();
        let until = pending.until;
        state.viewer.invalidate_viewport();
        let zoom = InputBounds::new(DesktopPoint { x: 100, y: 50 }, 160, 120).unwrap();
        let newer = state
            .viewer
            .configure_viewport(zoom, SurfaceRect::new(10, 10, 100, 100).unwrap())
            .unwrap();
        state.viewer.confirm_viewport(&newer).unwrap();
        let pending = state.viewer.pending.as_ref().unwrap();
        assert_eq!(pending.until, until);
        assert_eq!(&pending.bytes[..pending.len], original);
        assert_eq!(
            state.viewer.action_in_view(&sampled, button(false)),
            Err(Error::Backpressure)
        );
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let mut counters = (10000, 20000);
                receipt(
                    &mut state,
                    &client,
                    &host,
                    &mut counters,
                    SequenceSpace::Action,
                    0,
                )
                .await;
                assert_eq!(
                    state.effects.lock().unwrap().operations[0],
                    Op::Absolute(DesktopPoint { x: 160, y: 120 })
                );
                assert_eq!(
                    state.viewer.action_in_view(&sampled, button(false)),
                    Err(Error::Viewport(mapping::Error::Obsolete))
                );
                let top_left = newer.destination().origin();
                assert_eq!(
                    state
                        .viewer
                        .action_in_view(
                            &newer.at(LocalPoint::pixels(top_left.x, top_left.y)),
                            button(false)
                        )
                        .unwrap()
                        .sequence,
                    1
                );
                receipt(
                    &mut state,
                    &client,
                    &host,
                    &mut counters,
                    SequenceSpace::Action,
                    1,
                )
                .await;
                assert_eq!(
                    state.effects.lock().unwrap().operations[2],
                    Op::Absolute(DesktopPoint { x: 100, y: 50 })
                );
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
fn unconfirmed_retired_foreign_and_closed_layouts_cannot_consume_input_positions() {
    run(|client, host| async move {
        let mut state = Box::pin(mapped_fixture(&client, &host)).await;
        let area = SurfaceRect::new(0, 0, 320, 240).unwrap();
        let first = state.viewer.configure_viewport(bounds(), area).unwrap();
        let sampled = first.at(LocalPoint::pixels(10, 10));
        assert_eq!(
            state.viewer.pointer_in_view(&sampled),
            Err(Error::Viewport(mapping::Error::Unconfirmed))
        );
        assert!(!state.viewer.pending_send());
        state.viewer.confirm_viewport(&first).unwrap();
        let second = state.viewer.configure_viewport(bounds(), area).unwrap();
        assert_eq!(
            state.viewer.confirm_viewport(&first),
            Err(Error::Viewport(mapping::Error::Obsolete))
        );
        let mut other = state.viewer.input.viewport();
        let foreign = other.configure(bounds(), area).unwrap();
        assert_eq!(
            state.viewer.confirm_viewport(&foreign),
            Err(Error::Viewport(mapping::Error::Obsolete))
        );
        state.viewer.confirm_viewport(&second).unwrap();
        assert_eq!(
            state.viewer.pointer_in_view(&sampled),
            Err(Error::Viewport(mapping::Error::Obsolete))
        );
        assert_eq!(
            state
                .viewer
                .pointer_in_view(&second.at(LocalPoint::pixels(10, 10)))
                .unwrap()
                .sequence,
            0
        );
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                receipt(
                    &mut state,
                    &client,
                    &host,
                    &mut (10000, 20000),
                    SequenceSpace::Pointer,
                    0,
                )
                .await;
                assert_eq!(state.effects.lock().unwrap().operations.len(), 1);
                state.viewer.control().stop();
                assert_eq!(state.viewer.confirm_viewport(&second), Err(Error::Closed));
                assert_eq!(
                    state.viewer.configure_viewport(bounds(), area).err(),
                    Some(Error::Closed)
                );
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
    });
}

#[test]
fn outside_image_release_is_not_clamped_and_actual_held_snapshot_releases_drag() {
    run(|client, host| async move {
        let mut state = Box::pin(mapped_fixture(&client, &host)).await;
        let layout = window(&mut state);
        let inside = layout.at(LocalPoint::pixels(200, 210));
        let driver = state.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let mut counters = (10000, 20000);
                assert_eq!(
                    state
                        .viewer
                        .action_in_view(&inside, button(true))
                        .unwrap()
                        .sequence,
                    0
                );
                receipt(
                    &mut state,
                    &client,
                    &host,
                    &mut counters,
                    SequenceSpace::Action,
                    0,
                )
                .await;
                assert_eq!(
                    state
                        .viewer
                        .action_in_view(&layout.at(LocalPoint::pixels(5, 5)), button(false)),
                    Err(Error::Viewport(mapping::Error::OutsideImage))
                );
                assert!(!state.viewer.pending_send());
                assert_eq!(state.effects.lock().unwrap().operations.len(), 2);
                state.viewer.invalidate_viewport();
                let snapshot = state
                    .viewer
                    .reconcile_held(HeldState::empty())
                    .unwrap()
                    .unwrap();
                assert_eq!(snapshot.next_action, 1);
                let end = now(&client).unwrap() + 800_000;
                while state.effects.lock().unwrap().operations.len() < 3 {
                    assert!(now(&client).unwrap() < end);
                    let (left, right) =
                        turn(&mut state, &client, &host, &mut counters.0, &mut counters.1).await;
                    left.unwrap();
                    right.unwrap();
                }
                assert_eq!(
                    state.effects.lock().unwrap().operations[2],
                    Op::Button {
                        button: PointerButton::Primary,
                        pressed: false
                    }
                );
                // No move to a clamped edge and no consumed action for the refusal.
                let next = window(&mut state);
                assert_eq!(
                    state
                        .viewer
                        .action_in_view(&next.at(LocalPoint::pixels(200, 210)), button(true))
                        .unwrap()
                        .sequence,
                    1
                );
                receipt(
                    &mut state,
                    &client,
                    &host,
                    &mut counters,
                    SequenceSpace::Action,
                    1,
                )
                .await;
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn local_layout_confirmation_cannot_replace_visibility_or_receiver_lifetime() {
    run(|client, host| async move {
        let mut state = Box::pin(mapped_fixture(&client, &host)).await;
        let layout = window(&mut state);
        submit_successor(&mut state, &client);
        state.viewer.confirm_viewport(&layout).unwrap();
        assert_eq!(
            state
                .viewer
                .pointer_in_view(&layout.at(LocalPoint::pixels(200, 210))),
            Err(Error::View(presentation::Error::Input(
                fr_client::input::Error::NoPresentedView
            )))
        );
        assert!(!state.viewer.pending_send());
        state.viewer.visible(1).unwrap();
        assert_eq!(
            state
                .viewer
                .pointer_in_view(&layout.at(LocalPoint::pixels(200, 210)))
                .unwrap()
                .sequence,
            0
        );
        state.receiver.close();
        assert!(state.viewer.confirm_viewport(&layout).is_err());
        assert!(state.viewer.is_closed());
        assert!(!state.viewer.pending_send());
        assert_eq!(state.effects.lock().unwrap().operations, [] as [Op; 0]);
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
