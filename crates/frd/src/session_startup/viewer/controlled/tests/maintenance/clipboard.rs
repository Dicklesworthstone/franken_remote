//! Optional retirement on the real connection during a compositor gap. The
//! one-use worker seed never opens a native clipboard; no payload is invented.
use super::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    Live,
    WorkerStop,
    LocalDisable,
}

#[test]
fn clipboard_maintenance_during_a_visibility_gap_retires_only_unavailable_owners() {
    for ending in [Ending::Live, Ending::WorkerStop, Ending::LocalDisable] {
        run(move |c, h| async move {
            let mut state = Box::pin(fixture_with_clipboard(
                &c,
                &h,
                caps(),
                false,
                false,
                false,
                ClipboardMode::Attached,
            ))
            .await;
            // Retain both actual negotiated owners for the whole fixture.
            let (_host_channel, channel) = state.clipboard_channels.take().unwrap();
            let seed = state.viewer.attach_clipboard(channel, true).unwrap();
            let (local, _) = state.viewer.clipboard_switches().unwrap();
            let lease = state.viewer.input.binding().lease;
            submit_successor(&mut state, &c);
            match ending {
                Ending::Live => {}
                Ending::WorkerStop => seed.control().stop(),
                Ending::LocalDisable => local.set_enabled(false),
            }
            state
                .viewer
                .drive(Duration::ZERO, |_| {}, block)
                .await
                .unwrap();
            assert!(!state.viewer.is_closed());
            assert_eq!(state.viewer.clipboard_retired(), ending != Ending::Live);
            assert_eq!(
                state.viewer.clipboard_reason().is_some(),
                ending != Ending::Live
            );
            assert_eq!(state.viewer.input.binding().lease, lease);
            assert_eq!(state.viewer.pending_actions(), 0);
            assert!(state.viewer.take_clipboard_received().unwrap().is_none());
            assert_eq!(state.effects.lock().unwrap().keys, [] as [bool; 0]);
            assert!(matches!(
                state
                    .viewer
                    .input
                    .view_deadline(ClientInstant(now(&c).unwrap())),
                Err(presentation::Error::Input(
                    fr_client::input::Error::NoPresentedView
                ))
            ));
            // Re-enabling a retired switch does not resurrect its worker or
            // original reservation. Desktop input independently resumes only
            // after the actual visibility callback for this exact picture.
            if ending == Ending::LocalDisable {
                local.set_enabled(true);
                state.viewer.service_clipboard().unwrap();
                assert!(state.viewer.clipboard_retired());
            }
            state.viewer.visible(1).unwrap();
            assert_eq!(state.viewer.action(key(true)).unwrap().sequence, 0);
            assert_eq!(state.viewer.input.binding().lease, lease);
            assert!(!state.viewer.is_closed());
            cleanup(&mut state).await;
            drop(seed);
        });
    }
}
