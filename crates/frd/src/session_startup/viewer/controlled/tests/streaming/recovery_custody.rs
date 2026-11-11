//! Actual controlled TLS/UDP, action receipts and the canonical recovery loop.
//! Codec output, held-state effects and native capture cleanup are fixtures.
use super::*;
use crate::session_startup::{
    StreamingViewerError,
    viewer::controlled::events::{CaptureCleanup, CaptureReapError, NativeCapture},
};
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

struct Capture {
    control: ViewerControl,
    stopped: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}
impl NativeCapture for Capture {
    fn stop(&self) {
        assert!(self.control.is_stopped(), "input must be fenced first");
        self.stopped.store(true, Ordering::Release);
    }
    fn try_reap(&mut self) -> bool {
        self.reaped.load(Ordering::Acquire)
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
        self.dropped.store(true, Ordering::Release);
    }
}

/// Announce a real missing dependency to the existing receiver. Its original
/// reference deadline, rather than a forced timestamp, starts recovery.
fn lose_reference(f: &mut Fixture, cx: &Cx) {
    let at = now(cx).unwrap();
    let mut bytes = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                total_bytes: 4,
                stride: 4,
                capture_micros: at,
            },
            observed_micros: at,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        f.host_media.bindings().for_channel(Channel::MediaConfig),
        &f.host_media.limits(),
        &mut bytes,
    )
    .unwrap();
    f.receiver
        .receive(Channel::MediaConfig, &bytes[..n], at)
        .unwrap();
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ending {
    ApplicationFailure,
    Abandoned,
}

#[test]
fn recovery_custody_survives_application_failure_after_media_detaches() {
    exercise(Ending::ApplicationFailure);
}

#[test]
fn recovery_custody_survives_abandoning_the_pending_offer_wait() {
    exercise(Ending::Abandoned);
}

#[allow(clippy::too_many_lines)]
fn exercise(ending: Ending) {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Box::pin(fixture_with_recovery(
            &c,
            &h,
            caps(),
            true,
            false,
            true,
            ClipboardMode::Disabled,
            true,
        ))
        .await;
        let driver = f.driver.take().unwrap();
        let ((), summary) = Box::pin(support::both(
            async {
                // Keep a genuine acknowledged effect in the original ledger.
                // A failed recovery must not erase this receipt or replay it.
                let _ = f.viewer.action(key(true)).unwrap();
                let mut nonces = 140_000;
                let mut tickets = 150_000;
                let until = now(&c).unwrap() + 1_000_000;
                while f.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until, "missing original action receipt");
                    let (host, viewer) = turn(&mut f, &c, &h, &mut nonces, &mut tickets).await;
                    host.unwrap();
                    viewer.unwrap();
                }
                let receipt = f.viewer.last_result();
                assert!(receipt.is_some());
                assert_eq!(f.effects.lock().unwrap().keys, [true]);
                let binding = f.viewer.input_result_binding();
                let control = f.viewer.control();
                let stopped = Arc::new(AtomicBool::new(false));
                let reaped = Arc::new(AtomicBool::new(false));
                let dropped = Arc::new(AtomicBool::new(false));
                f.viewer.native_capture = Some(Box::new(Capture {
                    control: control.clone(),
                    stopped: stopped.clone(),
                    reaped: reaped.clone(),
                    dropped: dropped.clone(),
                }));
                lose_reference(&mut f, &c);
                let mut viewer = f
                    .viewer
                    .into_streaming(f.presenter.take().unwrap(), f.receiver)
                    .unwrap();
                let worker = viewer.worker_id();
                let recovering = Cell::new(false);
                let done = Cell::new(false);
                let reports = Cell::new(0);
                let after = now(&c).unwrap() + 1_000_000;
                let repair =
                    Route::Stream(f.host_media.repair_stream(f.host.io().unwrap().0).unwrap());
                let (result, ()) = Box::pin(support::both(
                    async {
                        let mut serving = Box::pin(viewer.serve(
                            |owner, event| {
                                assert!(now(&c).unwrap() < after, "recovery did not begin");
                                assert!(event.is_none());
                                let owner = owner.expect("original controller disappeared");
                                assert_eq!(owner.input_result_binding(), binding);
                                if owner.media.is_none() {
                                    assert!(owner.input.suspended_since().is_some());
                                    if reports.get() == 1 {
                                        recovering.set(true);
                                        if ending == Ending::ApplicationFailure {
                                            return Err(());
                                        }
                                    }
                                }
                                Ok(())
                            },
                            |_| panic!("recovery must not invent or replay an action receipt"),
                            block,
                        ));
                        let result = poll_fn(|cx| {
                            let state = serving.as_mut().poll(cx);
                            if recovering.get() && ending == Ending::Abandoned {
                                assert!(state.is_pending());
                                Poll::Ready(None)
                            } else {
                                state.map(Some)
                            }
                        })
                        .await;
                        // Abandon a POLLED recovery with its original controller
                        // borrowed by the next-offer wait, not an unpolled serve.
                        drop(serving);
                        done.set(true);
                        result
                    },
                    async {
                        while !done.get() {
                            let result = f
                                .host
                                .drive(
                                    Duration::from_millis(1),
                                    || nonce(&mut nonces),
                                    || {
                                        tickets += 1;
                                        Some(InputTicketId::from_raw(tickets))
                                    },
                                    |route, bytes| {
                                        if bytes.get(6..8)
                                            == Some(&(Kind::RecoveryRequest as u16).to_be_bytes())
                                        {
                                            reports.set(reports.get() + 1);
                                            return Ok(Disposition::Consumed);
                                        }
                                        if route == repair {
                                            return Ok(Disposition::Consumed);
                                        }
                                        block(route, bytes)
                                    },
                                )
                                .await;
                            if done.get() {
                                break;
                            }
                            result.unwrap();
                        }
                        f.host.close();
                    },
                ))
                .await;
                assert!(recovering.get());
                assert_eq!(reports.get(), 1);
                match ending {
                    Ending::ApplicationFailure => {
                        assert_eq!(result, Some(Err(StreamingViewerError::Application)));
                    }
                    Ending::Abandoned => assert!(result.is_none()),
                }
                assert!(control.is_stopped());
                assert!(stopped.load(Ordering::Acquire));
                assert!(!dropped.load(Ordering::Acquire));
                assert_eq!(viewer.last_result(), receipt);
                assert_eq!(viewer.pending_control_actions(), 0);
                assert_eq!(viewer.worker_id(), worker);
                assert_eq!(viewer.input_capture_cleanup(), CaptureCleanup::Pending);
                // A timed-out collection must retain the same native producer;
                // only its actual completion can change Pending to Complete.
                assert_eq!(
                    viewer
                        .reap_input_capture(
                            &cleanup,
                            crate::worker::Deadline::after(&cleanup, Duration::from_millis(2))
                                .unwrap(),
                        )
                        .await,
                    Err(CaptureReapError::Expired)
                );
                assert!(!dropped.load(Ordering::Acquire));
                reaped.store(true, Ordering::Release);
                assert_eq!(
                    viewer
                        .reap_input_capture(
                            &cleanup,
                            crate::worker::Deadline::after(&cleanup, Duration::from_secs(1))
                                .unwrap(),
                        )
                        .await,
                    Ok(CaptureCleanup::Complete)
                );
                viewer
                    .reap_media(
                        &cleanup,
                        crate::worker::Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(viewer.last_result(), receipt);
                assert_eq!(viewer.input_capture_cleanup(), CaptureCleanup::Complete);
                f.observation.revoke();
            },
            driver,
        ))
        .await;
        assert!(summary.handoff_safe());
    });
}
