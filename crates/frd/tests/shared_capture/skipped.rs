//! Real source/worker/egress owners with synthetic completed native drops.
use super::*;
use fr_media::access_unit::{FrameId, FrameKind};

pub(super) async fn skipping_source(
    owner: &ObservationControl,
    skips: i32,
    same_original: bool,
) -> CaptureSource {
    let file = image(
        "fr-capture-skipped",
        include_str!("../support/capture_skipped_fixture.py")
            .replace("@SKIPS@", &skips.to_string())
            .replace(
                "@SAME_ORIGINAL@",
                if same_original { "True" } else { "False" },
            ),
    );
    let mut config = configuration();
    config.backend = fr_media::worker::Backend::VideoToolbox;
    CaptureSource::start(
        owner,
        Launch::new(file, ":0", None, Role::Capture, 78).unwrap(),
        config,
    )
    .await
    .unwrap()
}

#[test]
fn skipped_shared_result_refunds_capacity_without_progress_and_retains_the_reference() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (viewer, _) = gate(&rt, 2);
        let mut source = skipping_source(&owner, 3, false).await;
        let p = SharedFramePool::new(ProtocolLimits::ABSOLUTE, 4 * 1024 * 1024, 4).unwrap();
        let mut sender = egress(&viewer, limits(1150), bindings(1), SendPolicy::default());
        let mut r = receiver(&cx, limits(1150), bindings(1));
        let first = source
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let reference = first.frame();
        sender.enqueue_shared_capture(&first).unwrap();
        pump(&cx, &mut sender, &mut r);
        decoded(&cx, &mut r);
        drop(first);
        let usage = p.usage();
        let cached = sender.cache_usage();
        for _ in 0..3 {
            let prepared = source.prepare_shared_capture(&owner, &p).unwrap();
            assert!(p.usage().bytes > usage.bytes);
            let skipped = prepared.capture_if_changed(false).await.unwrap();
            assert!(skipped.is_skipped());
            assert!(!skipped.is_unchanged());
            assert_eq!(skipped.observed_micros(), None);
            assert_eq!(skipped.frame(), reference);
            assert!(skipped.encoded().is_none());
            assert_eq!(p.usage(), usage);
            sender.enqueue_shared_capture(&skipped).unwrap();
            assert_eq!(sender.cache_usage(), cached);
            assert!(pump(&cx, &mut sender, &mut r).is_empty());
            let mut fresh = egress(&viewer, limits(1150), bindings(21), SendPolicy::default());
            assert_eq!(
                fresh.enqueue_shared_capture(&skipped),
                Err(Error::InvalidFrame)
            );
        }
        let next = source
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(false)
            .await
            .unwrap();
        assert_eq!(next.frame(), FrameId::from_raw(4));
        assert_eq!(
            next.encoded().unwrap().kind(),
            FrameKind::Predicted {
                references: reference
            }
        );
        sender.enqueue_shared_capture(&next).unwrap();
        assert!(!pump(&cx, &mut sender, &mut r).is_empty());
        stop(&mut source, &cx).await;
    });
}

#[test]
fn skipped_private_result_never_becomes_source_freshness_or_a_reference() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let (viewer, _) = gate(&rt, 2);
        let mut source = skipping_source(&owner, 1, false).await;
        let mut sender = egress(&viewer, limits(1150), bindings(1), SendPolicy::default());
        let mut r = receiver(&cx, limits(1150), bindings(1));
        let first = source.capture_if_changed(&owner, true).await.unwrap();
        let reference = first.frame();
        sender.enqueue_capture(first).unwrap();
        pump(&cx, &mut sender, &mut r);
        decoded(&cx, &mut r);
        let skipped = source.capture_if_changed(&owner, false).await.unwrap();
        assert!(skipped.is_skipped());
        assert_eq!(skipped.frame(), reference);
        assert_eq!(skipped.observed_micros(), None);
        sender.enqueue_capture(skipped).unwrap();
        assert!(pump(&cx, &mut sender, &mut r).is_empty());
        let next = source.capture_if_changed(&owner, false).await.unwrap();
        assert_eq!(next.frame(), FrameId::from_raw(2));
        assert_eq!(
            next.encoded().unwrap().kind(),
            FrameKind::Predicted {
                references: reference
            }
        );
        sender.enqueue_capture(next).unwrap();
        stop(&mut source, &cx).await;
    });
}

#[test]
fn required_picture_retries_new_candidates_under_the_original_timestamp() {
    run_shared!(rt, cx, {
        for force in [false, true] {
            let (owner, _) = gate(&rt, if force { 2 } else { 1 });
            let mut source = skipping_source(&owner, 2, true).await;
            source.capture_if_changed(&owner, true).await.unwrap();
            let start = host_now(&cx).unwrap().as_micros();
            let frame = if force {
                source
                    .capture_if_changed(&owner, true)
                    .await
                    .unwrap()
                    .encoded()
                    .unwrap()
                    .frame()
            } else {
                source.capture(&owner, false).await.unwrap().frame()
            };
            assert_eq!(frame, FrameId::from_raw(3));
            let elapsed = host_now(&cx).unwrap().as_micros() - start;
            assert!(elapsed >= 2 * 1_000_000_u64.div_ceil(30));
            assert!(elapsed < 2_000_000);
            stop(&mut source, &cx).await;
        }
    });
}

#[test]
fn perpetual_drops_cannot_renew_a_required_picture_deadline() {
    run_shared!(rt, cx, {
        let (owner, _) = gate(&rt, 1);
        let mut source = skipping_source(&owner, -1, true).await;
        source.capture_if_changed(&owner, true).await.unwrap();
        let start = host_now(&cx).unwrap().as_micros();
        let error = source.capture_if_changed(&owner, true).await.unwrap_err();
        assert_eq!(error, Error::Worker(frd::worker::Error::Deadline));
        let elapsed = host_now(&cx).unwrap().as_micros() - start;
        assert!((1_900_000..3_000_000).contains(&elapsed));
        let deadline = Deadline::after(&cx, Duration::from_secs(1)).unwrap();
        source.worker_mut().reap(&cx, deadline).await.unwrap();
    });
}
