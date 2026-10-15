//! Real supervised pipes; the child verifies staged cursor bytes but does not
//! decode HEVC or render a window. No cursor reply is a presentation receipt.
use super::*;
use crate::media::presentation::tests::{configuration, presenter};
use asupersync::{cx::Cx, runtime::RuntimeBuilder};
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::delivery::{
    MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline, ReceivePolicy,
};
use fr_wire::MediaLimits;
use std::{future::poll_fn, pin::pin, task::Poll};

async fn fixture(cx: &Cx, mode: &str) -> (Presenter, ReceivePipeline) {
    let limits = MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16384, 64).unwrap();
    let mut receiver = ReceivePipeline::new(
        ReceiveConfig {
            limits,
            bindings: MediaBindings::new(1, 2, 3, 4).unwrap(),
            epoch: MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            policy: ReceivePolicy::default(),
        },
        MediaBudget::new(limits.protocol()).unwrap(),
    )
    .unwrap();
    let presenter = presenter(cx, &mut receiver, mode).await;
    assert_eq!(presenter.configuration, configuration());
    (presenter, receiver)
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
async fn reap(presenter: &mut Presenter, cx: &Cx) {
    presenter.abort();
    presenter
        .reap(
            cx,
            worker::Deadline::after(cx, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
}

#[test]
fn prepared_redraw_releases_the_cursor_cache_borrow_and_sends_the_original_bytes() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, receiver) = fixture(&cx, "cursor-snapshot").await;
        let state = receiver.state();
        let usage = receiver.budget_usage();
        let worker = presenter.worker_id();
        let mut rgba = vec![1, 2, 3, 255];
        let pending = {
            let update = overlay::Update::Shape {
                x: 20,
                y: 30,
                width: 1,
                height: 1,
                hotspot_x: 0,
                hotspot_y: 0,
                rgba: &rgba,
            };
            presenter.apply_cursor(&cx, &update)
        };
        // The cache and the borrowed Update can change/drop before IPC starts.
        // The child refuses anything other than the original staged pixels.
        rgba.fill(99);
        drop(rgba);
        pending.await.unwrap();
        assert_eq!(presenter.worker_id(), worker);
        assert_eq!(presenter.worker.state(), worker::State::Running);
        assert_eq!(receiver.state(), state);
        assert_eq!(receiver.budget_usage(), usage);
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn invalid_redraw_refuses_before_native_submission_without_poisoning_a_healthy_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, _) = fixture(&cx, "cursor-snapshot").await;
        let update = overlay::Update::Shape {
            x: 0,
            y: 0,
            width: 0,
            height: 0,
            hotspot_x: 0,
            hotspot_y: 0,
            rgba: &[],
        };
        assert!(presenter.apply_cursor(&cx, &update).await.is_err());
        assert_eq!(presenter.worker.state(), worker::State::Running);
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn invalid_native_redraw_reply_and_started_abandonment_poison_the_original_worker() {
    for mode in ["cursor-bad", "cursor-stall"] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let (mut presenter, _) = fixture(&cx, mode).await;
            if mode == "cursor-bad" {
                assert!(
                    presenter
                        .apply_cursor(&cx, &overlay::Update::Hidden)
                        .await
                        .is_err()
                );
            } else {
                let mut pending = pin!(presenter.apply_cursor(&cx, &overlay::Update::Hidden));
                assert!(
                    poll_fn(|task| Poll::Ready(pending.as_mut().poll(task).is_pending())).await
                );
            }
            assert_eq!(presenter.worker.state(), worker::State::Poisoned);
            reap(&mut presenter, &cx).await;
        });
    }
}
