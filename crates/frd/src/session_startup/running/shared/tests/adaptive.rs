//! Actual native UDP/TLS/QUIC, original source IPC and shared ownership. The
//! capture/codec replies and presentation completions remain explicit fixtures.
use super::*;
use crate::media::shared_publisher::Error as PublisherError;
use fr_media::pacing::{Mode, Reason};

#[test]
fn adaptive_shared_source_reaches_verified_idle_and_keeps_the_remaining_viewer_live() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut group = Box::pin(group(&rt, 2)).await;
        ready(&mut group).await;
        group
            .publisher
            .enable_adaptive_capture(Duration::from_millis(50), Duration::from_millis(200))
            .unwrap();
        let original = group.publisher.worker_id();
        let started = now(&group.peers[0].h).unwrap();
        let mut reports = 0;
        let source = group.publisher.serve(Duration::from_millis(50), |report| {
            assert!(
                report.unchanged,
                "idle checks must not encode dummy pictures"
            );
            reports += 1;
        });
        let network = async {
            let (first, second) = group.peers.split_at_mut(1);
            let first = &mut first[0];
            let second = &mut second[0];
            while now(&second.h).unwrap() < started + 1_600_000 {
                let (a, b) = Box::pin(support::both(first.turn(), second.turn())).await;
                a.unwrap();
                b.unwrap();
            }
            first.shared.as_mut().unwrap().close();
            assert!(first.control.check().is_err());
            while now(&second.h).unwrap() < started + 3_100_000 {
                second.turn().await.unwrap();
            }
            assert!(second.control.check().is_ok());
            assert!(
                second
                    .shared
                    .as_ref()
                    .unwrap()
                    .renewed_until()
                    .unwrap()
                    .as_micros()
                    > started + 3_000_000
            );
            assert_eq!(second.frames, [0]);
            second.shared.as_mut().unwrap().close();
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert_eq!(result, Err(PublisherError::Closed));
        let pacing = group.publisher.pacing().unwrap();
        assert_eq!(pacing.report().unwrap().mode, Mode::Idle);
        assert_eq!(pacing.interval_us(), 200_000);
        assert!(pacing.decisions().any(|r| r.reason == Reason::VerifiedIdle));
        assert!(
            (10..40).contains(&reports),
            "bounded idle checks: {reports}"
        );
        assert_eq!(group.publisher.worker_id(), original);
        assert!(group.owner.check().is_err());
        cleanup(&mut group, &cleanup_cx).await;
    });
}

#[test]
fn invalid_or_mismatched_shared_pacing_never_issues_a_native_capture() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut group = Box::pin(group(&rt, 1)).await;
        ready(&mut group).await;
        assert!(
            group
                .publisher
                .enable_adaptive_capture(Duration::from_millis(10), Duration::from_millis(200),)
                .is_err()
        );
        assert!(group.publisher.pacing().is_none());
        group
            .publisher
            .enable_adaptive_capture(Duration::from_millis(50), Duration::from_millis(200))
            .unwrap();
        assert!(
            group
                .publisher
                .enable_adaptive_capture(Duration::from_millis(50), Duration::from_millis(200),)
                .is_err()
        );
        let result = group
            .publisher
            .serve(Duration::from_millis(60), |_| {
                panic!("invalid cadence reached capture");
            })
            .await;
        assert_eq!(result, Err(PublisherError::InvalidBudget));
        assert!(group.owner.check().is_err());
        cleanup(&mut group, &cleanup_cx).await;
    });
}
