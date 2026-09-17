//! Original negotiated UDP/TLS viewer; the native lifetime is an explicit test
//! adapter. Native X11 event semantics have their separate required Xvfb lane.
use super::*;
use crate::session_startup::viewer_events::{
    CaptureCleanup, CaptureStartError, Layout, NativeCapture, Source,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Default)]
struct NativeState {
    stops: AtomicUsize,
    finished: AtomicBool,
    drops: AtomicUsize,
}
struct Native {
    source: Source,
    state: Arc<NativeState>,
}
impl NativeCapture for Native {
    fn stop(&self) {
        // A native adapter must be called AFTER the original authority fence.
        assert!(self.source.control().is_stopped());
        self.state.stops.fetch_add(1, Ordering::AcqRel);
    }
    fn try_reap(&mut self) -> bool {
        self.state.finished.load(Ordering::Acquire)
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        assert!(self.source.control().is_stopped());
        self.state.drops.fetch_add(1, Ordering::AcqRel);
    }
}
fn own(viewer: &mut ControlledViewer, layout: &Layout) -> Arc<NativeState> {
    let state = Arc::new(NativeState::default());
    viewer
        .capture_input_owned(layout, |mut source| {
            // This event remains in the ORIGINAL input queue, never sent through
            // a parallel native or transport path during factory setup.
            let sampled = source.clock().unwrap();
            source
                .push(physical(KeyTransition::Press), sampled)
                .unwrap();
            Ok::<_, ()>(Native {
                source,
                state: state.clone(),
            })
        })
        .unwrap();
    state
}
#[test]
fn owned_capture_requires_this_confirmed_layout_and_only_one_source() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        let layout = state
            .viewer
            .configure_viewport(bounds(), SurfaceRect::new(0, 0, 320, 240).unwrap())
            .unwrap();
        let never = |_: Source| -> Result<Native, ()> { panic!("native factory must not run") };
        assert_eq!(
            state.viewer.capture_input_owned(&layout, never),
            Err(CaptureStartError::Viewer(Error::Viewport(
                fr_client::input::viewport::Error::Unconfirmed
            )))
        );
        let retired = layout.clone();
        let layout = state
            .viewer
            .configure_viewport(bounds(), SurfaceRect::new(0, 0, 320, 240).unwrap())
            .unwrap();
        state.viewer.confirm_viewport(&layout).unwrap();
        assert_eq!(
            state.viewer.capture_input_owned(&retired, never),
            Err(CaptureStartError::Viewer(Error::Viewport(
                fr_client::input::viewport::Error::Obsolete
            )))
        );
        let native = own(&mut state.viewer, &layout);
        assert_eq!(
            state.viewer.capture_input_owned(&layout, never),
            Err(CaptureStartError::Viewer(Error::Capture(
                events::Error::AlreadyAttached
            )))
        );
        assert!(!state.viewer.is_closed());
        assert_eq!(native.stops.load(Ordering::Acquire), 0);
        state.viewer.close();
        assert_eq!(state.effects.lock().unwrap().operations, []);
        state.host.close();
    });
}
#[test]
fn unpolled_drive_abandonment_fences_native_without_waiting_and_retains_cleanup() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        assert_eq!(
            state.viewer.input_capture_cleanup(),
            CaptureCleanup::NotStarted
        );
        let layout = layout(&mut state);
        let native = own(&mut state.viewer, &layout);
        assert_eq!(
            state.viewer.input_capture_cleanup(),
            CaptureCleanup::Pending
        );
        drop(state.viewer.drive(Duration::from_millis(1), |_| {}, block));
        assert!(state.viewer.is_closed());
        assert!(native.stops.load(Ordering::Acquire) > 0);
        assert_eq!(native.drops.load(Ordering::Acquire), 0);
        assert_eq!(
            state.viewer.input_capture_cleanup(),
            CaptureCleanup::Pending
        );
        assert_eq!(state.effects.lock().unwrap().operations, []);
        native.finished.store(true, Ordering::Release);
        assert_eq!(
            state.viewer.input_capture_cleanup(),
            CaptureCleanup::Complete
        );
        assert_eq!(
            state.viewer.input_capture_cleanup(),
            CaptureCleanup::Complete
        );
        drop(state.viewer);
        assert_eq!(native.drops.load(Ordering::Acquire), 1);
        state.host.close();
    });
}
#[test]
fn failed_or_panicking_native_factory_cannot_leave_live_control() {
    for panic in [false, true] {
        run(|c, h| async move {
            let mut state = Box::pin(fixture(&c, &h)).await;
            let layout = layout(&mut state);
            let control = state.viewer.control();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                state.viewer.capture_input_owned(
                    &layout,
                    |source| -> Result<Native, &'static str> {
                        assert!(!source.control().is_stopped());
                        assert!(!panic, "explicit native factory panic fixture");
                        Err("native fixture unavailable")
                    },
                )
            }));
            if panic {
                assert!(result.is_err());
            } else {
                assert_eq!(
                    result.unwrap(),
                    Err(CaptureStartError::Native("native fixture unavailable"))
                );
            }
            assert!(control.is_stopped());
            assert!(state.viewer.is_closed());
            assert_eq!(
                state.viewer.input_capture_cleanup(),
                CaptureCleanup::NotStarted
            );
            assert_eq!(state.effects.lock().unwrap().operations, []);
            state.host.close();
        });
    }
}
#[test]
fn owned_capture_moves_with_original_stream_and_is_reapable_after_service_abandonment() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            receiver,
            presenter,
            observation,
            ..
        } = Box::pin(fixture_with_decoder(&c, &h, caps(), true)).await;
        let layout = viewer
            .configure_viewport(bounds(), SurfaceRect::new(0, 0, 320, 240).unwrap())
            .unwrap();
        viewer.confirm_viewport(&layout).unwrap();
        let native = own(&mut viewer, &layout);
        let mut stream = viewer.into_streaming(presenter.unwrap(), receiver).unwrap();
        assert_eq!(stream.input_capture_cleanup(), CaptureCleanup::Pending);
        assert_eq!(native.stops.load(Ordering::Acquire), 0);
        drop(stream.serve(|_, _| Ok(()), |_| {}, block));
        assert!(native.stops.load(Ordering::Acquire) > 0);
        assert_eq!(stream.input_capture_cleanup(), CaptureCleanup::Pending);
        native.finished.store(true, Ordering::Release);
        assert_eq!(stream.input_capture_cleanup(), CaptureCleanup::Complete);
        observation.revoke();
        host.close();
    });
}
