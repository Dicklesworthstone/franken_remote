//! Composed running receiver/controller; native/media/host reports are fixtures.
use super::*;
use crate::session_startup::{StreamingViewer, StreamingViewerError};
use std::ops::ControlFlow;

async fn media_fixture(c: &Cx, h: &Cx) -> Fixture {
    let mut f = Box::pin(fixture_with_decoder(c, h, caps(), true)).await;
    settle(&mut f).await;
    f
}
fn seed_delta(f: &mut Fixture, at: u64) {
    // Only the media bootstrap is injected. Closing uses actual original TLS/UDP.
    let limits = f.host_media.limits();
    let bindings = f.host_media.bindings();
    let descriptor = FrameDescriptor {
        frame: 1,
        reference: Some(0),
        total_bytes: 4,
        stride: 4,
        capture_micros: at,
    };
    let mut bytes = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor,
            observed_micros: at,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        bindings.for_channel(Channel::MediaConfig),
        &limits,
        &mut bytes,
    )
    .unwrap();
    f.receiver
        .receive(Channel::MediaConfig, &bytes[..n], at)
        .unwrap();
    let n = encode_fragment(
        Fragment {
            descriptor,
            index: 0,
            bytes: b"next",
        },
        bindings.for_channel(Channel::Video),
        &limits,
        &mut bytes,
    )
    .unwrap();
    f.receiver.receive(Channel::Video, &bytes[..n], at).unwrap();
}
async fn host_close(
    host: &mut ControlledHost,
    report: quic::RevocationReport,
) -> Option<Result<(), quic::Error>> {
    let mut nonces = 90_000;
    let mut tickets = 91_000;
    while host
        .drive(
            Duration::from_millis(1),
            || nonce(&mut nonces),
            || {
                tickets += 1;
                Some(InputTicketId::from_raw(tickets))
            },
            block,
        )
        .await
        .is_ok()
    {}
    report.finish().await
}
async fn reap(viewer: &mut StreamingViewer, cx: &Cx) {
    viewer
        .reap_media(
            cx,
            crate::worker::Deadline::after(cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
}
#[test]
fn streaming_disconnect_fences_both_native_owners_and_keeps_unresolved_actions() {
    run(|c, h| async move {
        let cleanup_cx = Cx::current().unwrap();
        let mut f = Box::pin(media_fixture(&c, &h)).await;
        let _ = f.viewer.action(key(true)).unwrap();
        let last = f.viewer.last_result();
        let (stopped, reaped) = capture(&mut f.viewer);
        let report = arm_report(&mut f, &cleanup_cx);
        seed_delta(&mut f, now(&c).unwrap());
        let picture = f
            .receiver
            .take_decodable(now(&c).unwrap())
            .unwrap()
            .unwrap();
        let charge = f.receiver.budget_usage();
        assert!(charge.bytes > 0);
        let mut viewer = f
            .viewer
            .into_streaming(f.presenter.take().unwrap(), f.receiver)
            .unwrap();
        let pid = viewer.worker_id();
        let ending =
            viewer.disconnect_control_with_cleanup(&cleanup_cx, closure::Reason::Requested);
        assert!(c.is_cancel_requested());
        assert!(stopped.load(Ordering::Acquire));
        assert!(!picture.is_live());
        let ((outcome, sent), summary) = Box::pin(support::both(
            support::both(ending, host_close(&mut f.host, report)),
            f.driver.take().unwrap(),
        ))
        .await;
        let outcome = outcome.unwrap();
        assert_eq!(outcome.exchange.transport, Ok(()));
        assert_eq!(sent, Some(Ok(())));
        assert!(summary.handoff_safe());
        assert!(outcome.revocation.is_some());
        assert_eq!(outcome.exchange.report, None);
        assert_eq!(viewer.control_disconnect_outcome(), Some(outcome));
        assert_eq!(viewer.pending_control_actions(), 1);
        assert_eq!(viewer.last_result(), last);
        assert_eq!(viewer.budget_usage(), charge);
        assert_eq!(viewer.worker_id(), pid);
        assert_eq!(viewer.input_capture_cleanup(), CaptureCleanup::Pending);
        drop(picture);
        assert_eq!(viewer.budget_usage(), BudgetUsage::default());
        assert!(
            viewer
                .disconnect_control_with_cleanup(&cleanup_cx, closure::Reason::Requested)
                .await
                .is_err()
        );
        assert_eq!(viewer.control_disconnect_outcome(), Some(outcome));
        reaped.store(true, Ordering::Release);
        assert_eq!(
            viewer
                .reap_input_capture(
                    &cleanup_cx,
                    crate::worker::Deadline::after(&cleanup_cx, Duration::from_secs(1)).unwrap()
                )
                .await,
            Ok(CaptureCleanup::Complete)
        );
        reap(&mut viewer, &cleanup_cx).await;
        assert_eq!(viewer.control_disconnect_outcome(), Some(outcome));
        assert_eq!(viewer.pending_control_actions(), 1);
        f.observation.revoke();
    });
}
async fn requested_from_loop(c: Cx, h: Cx, during_decode: bool) {
    let cleanup_cx = Cx::current().unwrap();
    let mut f = Box::pin(media_fixture(&c, &h)).await;
    let report = arm_report(&mut f, &cleanup_cx);
    let (stopped, reaped) = capture(&mut f.viewer);
    seed_delta(&mut f, now(&c).unwrap());
    let mut viewer = f
        .viewer
        .into_streaming(f.presenter.take().unwrap(), f.receiver)
        .unwrap();
    let mut turns = 0;
    let mut presentations = 0;
    let mut closing = false;
    let ((outcome, sent), summary) = Box::pin(support::both(
        support::both(
            viewer.serve_control_until(
                &cleanup_cx,
                |input, event| {
                    assert!(!closing, "no application callback after the close decision");
                    assert!(!stopped.load(Ordering::Acquire));
                    turns += 1;
                    presentations += usize::from(event.is_some());
                    let requested = if during_decode {
                        turns == 2
                    } else {
                        event.is_some()
                    };
                    if requested {
                        assert_eq!(event.is_none(), during_decode);
                        // Explicit local visibility fixture: native decoding alone
                        // does not authorize an input action on the new picture.
                        if let Some(event) = event {
                            input.visible(event.frame.as_raw()).unwrap();
                        }
                        // This pending action is never transmitted by the closing exchange.
                        let _ = input.action(key(true)).unwrap();
                        closing = true;
                        Ok(ControlFlow::Break(closure::Reason::Requested))
                    } else {
                        Ok(ControlFlow::Continue(()))
                    }
                },
                |_| panic!("no submitted action needs a result"),
                block,
            ),
            host_close(&mut f.host, report),
        ),
        f.driver.take().unwrap(),
    ))
    .await;
    let outcome = outcome.unwrap();
    assert_eq!(outcome.exchange.transport, Ok(()));
    assert_eq!(sent, Some(Ok(())));
    assert!(summary.handoff_safe());
    assert!(closing && stopped.load(Ordering::Acquire));
    assert_eq!(presentations, usize::from(!during_decode));
    assert_eq!(viewer.statistics().decoded, u64::from(!during_decode));
    assert_eq!(viewer.pending_control_actions(), 1);
    assert_eq!(viewer.budget_usage(), BudgetUsage::default());
    assert_eq!(viewer.control_disconnect_outcome(), Some(outcome));
    assert_eq!(f.effects.lock().unwrap().keys, [] as [bool; 0]);
    reaped.store(true, Ordering::Release);
    let _ = viewer
        .reap_input_capture(
            &cleanup_cx,
            crate::worker::Deadline::after(&cleanup_cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
    reap(&mut viewer, &cleanup_cx).await;
    f.observation.revoke();
}
#[test]
fn ui_close_fences_input_before_abandoning_the_inflight_decoder() {
    run(|c, h| requested_from_loop(c, h, true));
}
#[test]
fn ui_close_after_decoding_uses_the_same_controller_and_terminal_exchange() {
    run(|c, h| requested_from_loop(c, h, false));
}
#[test]
fn abandoned_stream_close_and_unpolled_service_still_retire_native_owners() {
    for service in [false, true] {
        run(|c, h| async move {
            let cleanup_cx = Cx::current().unwrap();
            let mut f = Box::pin(media_fixture(&c, &h)).await;
            let (stopped, _) = capture(&mut f.viewer);
            let mut viewer = f
                .viewer
                .into_streaming(f.presenter.take().unwrap(), f.receiver)
                .unwrap();
            if service {
                drop(viewer.serve_control_until(
                    &cleanup_cx,
                    |_, _| panic!("unpolled"),
                    |_| {},
                    block,
                ));
            } else {
                let closing =
                    viewer.disconnect_control_with_cleanup(&cleanup_cx, closure::Reason::Requested);
                assert!(stopped.load(Ordering::Acquire));
                drop(closing);
            }
            assert!(c.is_cancel_requested());
            assert!(stopped.load(Ordering::Acquire));
            assert_eq!(viewer.control_disconnect_outcome(), None);
            assert_eq!(viewer.budget_usage(), BudgetUsage::default());
            f.observation.revoke();
            assert!(f.driver.take().unwrap().await.handoff_safe());
            reap(&mut viewer, &cleanup_cx).await;
        });
    }
}
#[test]
fn callback_failure_and_emergency_stop_do_not_masquerade_as_orderly_closure() {
    for emergency in [false, true] {
        run(|c, h| async move {
            let cleanup_cx = Cx::current().unwrap();
            let mut f = Box::pin(media_fixture(&c, &h)).await;
            let mut viewer = f
                .viewer
                .into_streaming(f.presenter.take().unwrap(), f.receiver)
                .unwrap();
            let outcome = viewer
                .serve_control_until(
                    &cleanup_cx,
                    |input, _| {
                        if emergency {
                            input.control().stop();
                            Ok(ControlFlow::Continue(()))
                        } else {
                            Err(())
                        }
                    },
                    |_| {},
                    block,
                )
                .await;
            assert!(outcome.is_err());
            if !emergency {
                assert_eq!(outcome, Err(StreamingViewerError::Application));
            }
            assert_eq!(viewer.control_disconnect_outcome(), None);
            assert!(c.is_cancel_requested());
            f.observation.revoke();
            assert!(f.driver.take().unwrap().await.handoff_safe());
            reap(&mut viewer, &cleanup_cx).await;
        });
    }
}
#[test]
fn cancelled_application_context_cannot_become_the_streams_terminal_cleanup_context() {
    run(|c, h| async move {
        let cleanup_cx = Cx::current().unwrap();
        let mut f = Box::pin(media_fixture(&c, &h)).await;
        let mut viewer = f
            .viewer
            .into_streaming(f.presenter.take().unwrap(), f.receiver)
            .unwrap();
        let outcome = viewer
            .disconnect_control_with_cleanup(&c, closure::Reason::Requested)
            .await
            .unwrap();
        assert_eq!(outcome.exchange.transport, Err(quic::Error::Cancelled));
        assert_eq!(outcome.revocation, None);
        assert_eq!(outcome.exchange.report, None);
        assert_eq!(viewer.control_disconnect_outcome(), Some(outcome));
        assert_eq!(viewer.budget_usage(), BudgetUsage::default());
        f.observation.revoke();
        assert!(f.driver.take().unwrap().await.handoff_safe());
        reap(&mut viewer, &cleanup_cx).await;
    });
}
