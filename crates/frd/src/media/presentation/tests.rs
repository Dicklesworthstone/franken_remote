//! Actual supervised pipes and runtime; the decoder peer is explicitly synthetic.
use super::*;
use crate::worker::{Launch, Worker};
use asupersync::runtime::RuntimeBuilder;
use fr_core::{ids::*, limits::ProtocolLimits};
use fr_media::{
    delivery::*,
    worker::{Backend, Configuration, Role},
};
use fr_wire::{
    Channel, Fragment, FrameDescriptor, MediaLimits, RecoveryChunk, encode_fragment,
    encode_recovery,
};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) fn configuration() -> Configuration {
    Configuration {
        width: 320,
        height: 240,
        fps: 30,
        backend: Backend::SoftwareExplicit,
        bitrate: 2_000_000,
        max_access_unit_bytes: ProtocolLimits::ABSOLUTE.max_encoded_access_unit_bytes(),
        generation: CodecConfigurationGeneration::INITIAL,
    }
}
pub(crate) async fn presenter(cx: &Cx, receiver: &mut ReceivePipeline, mode: &str) -> Presenter {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let file: PathBuf = std::env::temp_dir().join(format!(
        "fr-decoder-{}-{}.py",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&file, include_str!("fixture.py").replace("@MODE@", mode)).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
    let configuration = configuration();
    let worker = Worker::start(
        cx,
        Launch::new(&file, ":0", None, Role::Present, 33).unwrap(),
        configuration,
        Deadline::after(cx, Duration::from_secs(1)).unwrap(),
    )
    .await
    .unwrap();
    let binding = receiver
        .bind_decoder(
            configuration.generation,
            &configuration.limits().unwrap(),
            host_now(cx).unwrap().as_micros(),
        )
        .unwrap();
    Presenter {
        worker,
        configuration,
        binding,
        stream_binding: None,
    }
}
fn runtime() -> asupersync::runtime::Runtime {
    RuntimeBuilder::new()
        .worker_threads(1)
        .blocking_threads(1, 2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
async fn fixture(cx: &Cx, mode: &str) -> (Presenter, ReceivePipeline, MediaLimits) {
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
    let mut bytes = [0; 1150];
    let n = encode_recovery(
        RecoveryChunk {
            frame: 0,
            total_bytes: 4,
            offset: 0,
            capture_micros: 5,
            bytes: b"fake",
        },
        2,
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(
            Channel::Recovery,
            &bytes[..n],
            host_now(cx).unwrap().as_micros(),
        )
        .unwrap();
    (presenter, receiver, limits)
}
fn next_frame(cx: &Cx, receiver: &mut ReceivePipeline, limits: MediaLimits) {
    let mut bytes = [0; 1150];
    let n = encode_fragment(
        Fragment {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                capture_micros: 6,
                total_bytes: 4,
                stride: 4,
            },
            index: 0,
            bytes: b"next",
        },
        1,
        &limits,
        &mut bytes,
    )
    .unwrap();
    receiver
        .receive(
            Channel::Video,
            &bytes[..n],
            host_now(cx).unwrap().as_micros(),
        )
        .unwrap();
}
async fn poll_once<F: Future>(f: std::pin::Pin<&mut F>) -> bool {
    let mut f = f;
    poll_fn(|cx| Poll::Ready(f.as_mut().poll(cx).is_pending())).await
}
#[test]
fn receiver_accepts_next_picture_while_native_decode_is_pending() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, limits) = fixture(&cx, "slow").await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        let first = receiver.budget_usage();
        let future = presenter.decode_job(&cx, job);
        let mut future = pin!(future);
        assert!(poll_once(future.as_mut()).await);
        next_frame(&cx, &mut receiver, limits);
        assert_eq!(receiver.budget_usage().pictures, first.pictures + 1);
        assert!(
            receiver
                .take_decodable(host_now(&cx).unwrap().as_micros())
                .unwrap()
                .is_none()
        );
        let done = future.await.unwrap();
        assert_eq!(receiver.budget_usage().pictures, 2);
        let receipt = done.complete(&cx, &mut receiver).unwrap();
        assert_eq!(receipt.frame.as_raw(), 0);
        assert_eq!(receiver.budget_usage().pictures, 1);
        let next = receiver
            .take_decodable(host_now(&cx).unwrap().as_micros())
            .unwrap()
            .unwrap();
        assert_eq!(next.bytes(), b"next");
        receiver
            .complete_decode(&next, host_now(&cx).unwrap().as_micros())
            .unwrap();
    });
}
#[test]
fn queued_decode_uses_original_display_deadline_not_dequeue_freshness() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = fixture(&cx, "healthy").await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        assert!(job.picture().within_display_queue_budget());
        let deadline = job.picture().display_deadline_us();
        sleep(cx.now(), Duration::from_millis(60)).await;
        let receipt = presenter
            .decode_job(&cx, job)
            .await
            .unwrap()
            .complete(&cx, &mut receiver)
            .unwrap();
        assert_eq!(receipt.stage, PresentationStage::DecodedOnly);
        assert_eq!(receipt.decoded.display_deadline_us(), deadline);
    });
}
#[test]
fn unpolled_decode_abandonment_fences_receiver_and_aborts_native_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = fixture(&cx, "healthy").await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        drop(presenter.decode_job(&cx, job));
        assert!(receiver.tick(host_now(&cx).unwrap().as_micros()).is_err());
        assert_eq!(receiver.state(), ReceiveState::Closed);
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert!(
            presenter
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .is_ok()
        );
    });
}
#[test]
fn receiver_failure_interrupts_native_wait_without_borrowing_decoder() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, _) = fixture(&cx, "stall").await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        {
            let mut future = pin!(presenter.decode_job(&cx, job));
            assert!(poll_once(future.as_mut()).await);
            receiver.close();
            assert!(future.await.is_err());
        }
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert!(
            presenter
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .is_ok()
        );
    });
}
#[test]
fn uncollected_native_completion_cannot_release_credit_or_enable_following_decode() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, limits) = fixture(&cx, "healthy").await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        let done = presenter.decode_job(&cx, job).await.unwrap();
        next_frame(&cx, &mut receiver, limits);
        assert_eq!(receiver.budget_usage().pictures, 2);
        assert!(presenter.take_next(&cx, &mut receiver).unwrap().is_none());
        drop(done);
        assert!(receiver.tick(host_now(&cx).unwrap().as_micros()).is_err());
        assert!(presenter.take_next(&cx, &mut receiver).is_err());
        assert_eq!(receiver.budget_usage().pictures, 0);
    });
}
#[test]
fn completion_for_replaced_receiver_is_rejected_without_closing_new_scope() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, limits) = fixture(&cx, "healthy").await;
        let job = presenter.take_next(&cx, &mut receiver).unwrap().unwrap();
        let done = presenter.decode_job(&cx, job).await.unwrap();
        receiver
            .replace(
                MediaEpoch {
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL.next().unwrap(),
                },
                MediaBindings::new(5, 6, 7, 8).unwrap(),
                host_now(&cx).unwrap().as_micros(),
            )
            .unwrap();
        let bound = receiver
            .bind_decoder(
                CodecConfigurationGeneration::INITIAL,
                limits.protocol(),
                host_now(&cx).unwrap().as_micros(),
            )
            .unwrap();
        assert!(done.complete(&cx, &mut receiver).is_err());
        assert!(bound.check(&receiver).is_ok());
        assert_eq!(receiver.state(), ReceiveState::AwaitingRecovery);
    });
}

#[test]
fn borrowed_present_next_abandonment_immediately_closes_and_drains_receiver() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, mut receiver, limits) = fixture(&cx, "stall").await;
        next_frame(&cx, &mut receiver, limits);
        assert_eq!(receiver.budget_usage().pictures, 2);
        {
            let mut operation = pin!(presenter.present_next(&cx, &mut receiver));
            assert!(poll_once(operation.as_mut()).await);
        }
        // No tick or subsequent packet is needed to complete borrowed cleanup.
        assert_eq!(receiver.state(), ReceiveState::Closed);
        assert_eq!(receiver.budget_usage(), BudgetUsage::default());
        assert_eq!(presenter.worker.state(), worker::State::Poisoned);
        assert!(
            presenter
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .is_ok()
        );
    });
}
