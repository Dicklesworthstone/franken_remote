//! The actual media presenter and supervised worker pipes. This synthetic peer
//! supplies viewport bytes; these tests make no native GPU or visibility claim.
use super::*;
use crate::media::presentation::tests::presenter;
use asupersync::runtime::{Runtime, RuntimeBuilder};
use fr_core::{
    ids::{CodecConfigurationGeneration, RecoveryGeneration},
    limits::ProtocolLimits,
};
use fr_media::{
    access_unit::FrameId,
    delivery::{
        MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig, ReceivePipeline, ReceivePolicy,
    },
};
use fr_wire::MediaLimits;
use std::time::Duration;

fn stamp() -> Stamp {
    Stamp {
        frame: FrameId::from_raw(17),
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    }
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}

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
    (presenter, receiver)
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
fn enlarged_viewport_preserves_the_original_source_and_receiver_evidence() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, receiver) = fixture(&cx, "snapshot-viewport").await;
        assert_eq!(presenter.picture(), (320, 240));
        let state = receiver.state();
        let usage = receiver.budget_usage();
        let worker = presenter.worker_id();
        let image = presenter
            .read_presentation(&cx, stamp())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(image.stamp(), stamp());
        assert_eq!((image.width(), image.height()), (640, 480));
        assert_eq!(image.pixels().len(), 640 * 480 * 4);
        assert!(
            image
                .pixels()
                .chunks_exact(4)
                .all(|p| p == [0x17, 0x2a, 0x3d, 255])
        );
        assert_eq!(presenter.picture(), (320, 240));
        assert_eq!(presenter.worker_id(), worker);
        assert_eq!(presenter.worker.state(), worker::State::Running);
        assert_eq!(receiver.state(), state);
        assert_eq!(receiver.budget_usage(), usage);
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn a_foreign_configuration_refuses_before_the_original_worker_is_used() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, _) = fixture(&cx, "snapshot-viewport").await;
        let mut wrong = stamp();
        wrong.configuration = CodecConfigurationGeneration::from_raw(99);
        assert_eq!(
            presenter.read_presentation(&cx, wrong).await.err(),
            Some(Error::InvalidFrame)
        );
        assert_eq!(presenter.worker.state(), worker::State::Running);
        assert!(
            presenter
                .read_presentation(&cx, stamp())
                .await
                .unwrap()
                .is_some()
        );
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn routine_export_absence_leaves_the_original_presenter_running() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let (mut presenter, receiver) = fixture(&cx, "snapshot-absent").await;
        let state = receiver.state();
        let usage = receiver.budget_usage();
        for _ in 0..2 {
            assert!(
                presenter
                    .read_presentation(&cx, stamp())
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(presenter.worker.state(), worker::State::Running);
            assert_eq!(receiver.state(), state);
            assert_eq!(receiver.budget_usage(), usage);
        }
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn enlarged_viewport_keeps_the_independent_byte_dimension_and_identity_limits() {
    for (mode, expected_error) in [
        (
            "snapshot-over-ceiling",
            Error::Worker(worker::Error::Protocol(
                fr_media::worker::Error::ResourceLimit,
            )),
        ),
        (
            "snapshot-wide",
            Error::Worker(worker::Error::Protocol(
                fr_media::worker::Error::ResourceLimit,
            )),
        ),
        (
            "snapshot-short",
            Error::Worker(worker::Error::Protocol(fr_media::worker::Error::Malformed)),
        ),
        ("snapshot-wrong-frame", Error::InvalidFrame),
        ("snapshot-wrong-configuration", Error::InvalidFrame),
        ("snapshot-wrong-recovery", Error::InvalidFrame),
    ] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let (mut presenter, _) = fixture(&cx, mode).await;
            assert_eq!(
                presenter.read_presentation(&cx, stamp()).await.err(),
                Some(expected_error),
                "{mode}"
            );
            assert_eq!(presenter.worker.state(), worker::State::Poisoned);
            reap(&mut presenter, &cx).await;
        });
    }
}
