//! Canonical host dispatch on real session/TLS/UDP owners. Decoder acknowledgements
//! and the remote loss report are explicit protocol fixtures, not native HEVC proof.
use super::*;
use crate::media::shared_publisher::RecoveryState;
use fr_media::delivery::MediaEpoch;
use fr_wire::recovery_request as wire;

fn request(p: &Member) -> Vec<u8> {
    let mut view = p.media.binding();
    view.parent = p.shared.as_ref().unwrap().session.binding();
    let mut bytes = vec![0; wire::REQUEST_BYTES];
    wire::encode(
        wire::Request {
            reason: wire::Reason::ReferenceExpired,
            last_useful_frame: Some(0),
        },
        view,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn network(p: &mut Member) -> Result<(), Error> {
    let (a, b) = Box::pin(support::both(
        p.shared
            .as_mut()
            .unwrap()
            .drive(Duration::from_millis(1), || nonce(&mut p.nonce), block),
        p.viewer.drive(Duration::from_millis(1), |_, bytes| {
            Ok(if feedback::is_feedback(bytes) {
                Disposition::Consumed
            } else {
                Disposition::Blocked
            })
        }),
    ))
    .await;
    a?;
    b?;
    asupersync::runtime::yield_now().await;
    Ok(())
}
async fn queue(p: &mut Member, bytes: &[u8]) {
    let until = now(&p.c).unwrap() + 500_000;
    loop {
        assert!(now(&p.c).unwrap() < until);
        let (q, routes) = p.viewer.io().unwrap();
        match q.send(&p.c, Route::Stream(routes.outbound), bytes, until, || true) {
            Ok(()) => return,
            Err(quic::Error::Backpressure) => network(p).await.unwrap(),
            error => panic!("request: {error:?}"),
        }
    }
}
async fn admit(p: &mut Member) -> Vec<u8> {
    let bytes = request(p);
    queue(p, &bytes).await;
    let until = now(&p.h).unwrap() + 1_000_000;
    while p.shared.as_ref().unwrap().statistics().recovery_requests == 0 {
        assert!(now(&p.h).unwrap() < until);
        network(p).await.unwrap();
    }
    bytes
}
fn deadline(p: &mut Member) -> u64 {
    let host = p.shared.as_mut().unwrap();
    host.subscriber
        .as_mut()
        .unwrap()
        .recovery_deadline(&host.session.opened.transport)
        .unwrap()
        .unwrap()
}

async fn replace(mut p: Member) -> Member {
    let until = deadline(&mut p);
    let Member {
        c,
        h,
        shared,
        mut viewer,
        media,
        mut receiver,
        control,
        mut nonce,
        frames,
        ..
    } = p;
    let mut host = shared.unwrap();
    let parent = host.session.binding();
    let (q, routes) = viewer.io().unwrap();
    let mut replacement = media
        .begin_replacement(&c, q, routes, parent, until, None, || true)
        .unwrap();
    while !replacement.is_complete()
        || host
            .subscriber
            .as_mut()
            .unwrap()
            .recovery_state(&host.session.opened.transport)
            .unwrap()
            != RecoveryState::DecoderStartup
    {
        assert!(now(&c).unwrap() < until);
        replacement
            .advance(&c, viewer.io().unwrap().0, || true)
            .unwrap();
        let (a, b) = Box::pin(support::both(
            host.drive(Duration::from_millis(1), || super::nonce(&mut nonce), block),
            viewer.drive(Duration::from_millis(1), |route, bytes| {
                if feedback::is_feedback(bytes) {
                    return Ok(Disposition::Consumed);
                }
                assert!(
                    replacement.owns_record(route, bytes) || route == Route::Stream(routes.inbound)
                );
                Ok(Disposition::Blocked)
            }),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    let media = replacement.finish(&c, viewer.io().unwrap().0).unwrap();
    assert_eq!(host.statistics().recovered_streams, 0);
    assert!(!host.startup_complete().unwrap());
    assert_eq!(
        host.subscriber
            .as_mut()
            .unwrap()
            .recovery_deadline(&host.session.opened.transport)
            .unwrap(),
        Some(until)
    );
    let view = media.binding();
    receiver
        .replace(
            MediaEpoch {
                configuration: view.configuration,
                recovery: view.recovery,
            },
            media.bindings(),
            now(&c).unwrap(),
        )
        .unwrap();
    let reply = media.decoder_routes_for_test().0;
    Member {
        c,
        h,
        shared: Some(host),
        viewer,
        media,
        receiver,
        control,
        nonce,
        frames,
        reply,
        session: None,
        host_media: None,
        configuration: None,
        configured: false,
        first: None,
        acknowledged: false,
    }
}

#[test]
fn original_shared_session_automatically_recovers_and_continues_with_healthy_peer() {
    let rt = support::runtime();
    let cleanup_cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 2, &[wire::CAPABILITY], true)).await;
        ready(&mut g).await;
        let worker = g.publisher.worker_id();
        let healthy_view = g.peers[1].media.binding();
        let original_connection = g.peers[0]
            .shared
            .as_ref()
            .unwrap()
            .session
            .opened
            .transport
            .binding();
        admit(&mut g.peers[0]).await;
        let failed = g.peers.remove(0);
        let frame = g.publisher.capture_next().await.unwrap();
        assert_eq!((frame.delivered, frame.refused), (1, 0));
        while !g.peers[0].frames.contains(&frame.frame) {
            g.peers[0].turn().await.unwrap();
        }
        let recovered = Box::pin(replace(failed)).await;
        g.peers.push(recovered);
        let idr = g.publisher.capture_next().await.unwrap();
        assert_eq!((idr.delivered, idr.refused), (2, 0));
        ready(&mut g).await;
        let recovered = &mut g.peers[1];
        let host = recovered.shared.as_ref().unwrap();
        assert!(
            host.session
                .opened
                .transport
                .is_bound_to(&original_connection)
        );
        assert_eq!(host.statistics().recovery_requests, 1);
        assert_eq!(host.statistics().recovered_streams, 1);
        assert_eq!(
            recovered.media.binding().recovery,
            healthy_view.recovery.next().unwrap()
        );
        assert!(recovered.frames.contains(&idr.frame));
        assert!(!recovered.control.view_ready().unwrap());
        assert_eq!(g.peers[0].media.binding(), healthy_view);
        let next = g.publisher.capture_next().await.unwrap();
        for p in &mut g.peers {
            while !p.frames.contains(&next.frame) {
                p.turn().await.unwrap();
            }
            assert!(p.control.check().is_ok());
        }
        assert_eq!(g.publisher.worker_id(), worker);
        cleanup(&mut g, &cleanup_cx).await;
    });
}

#[test]
fn duplicate_wire_request_during_attachment_does_not_recharge_or_extend_failure() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 2, &[wire::CAPABILITY], true)).await;
        ready(&mut g).await;
        let bytes = admit(&mut g.peers[0]).await;
        let original = deadline(&mut g.peers[0]);
        for _ in 0..3 {
            queue(&mut g.peers[0], &bytes).await;
            for _ in 0..8 {
                network(&mut g.peers[0]).await.unwrap();
            }
        }
        assert_eq!(deadline(&mut g.peers[0]), original);
        assert_eq!(
            g.peers[0]
                .shared
                .as_ref()
                .unwrap()
                .statistics()
                .recovery_requests,
            1
        );
        assert_eq!(
            g.peers[0]
                .shared
                .as_ref()
                .unwrap()
                .statistics()
                .recovered_streams,
            0
        );
        let frame = g.publisher.capture_next().await.unwrap();
        while !g.peers[1].frames.contains(&frame.frame) {
            g.peers[1].turn().await.unwrap();
        }
        assert!(g.peers[0].control.check().is_ok());
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn malformed_duplicate_recovery_request_closes_only_its_original_session() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 2, &[wire::CAPABILITY], true)).await;
        ready(&mut g).await;
        let mut bytes = admit(&mut g.peers[0]).await;
        // Keep the envelope kind, but corrupt its exact remote-session identity.
        bytes[63] ^= 1;
        queue(&mut g.peers[0], &bytes).await;
        let until = now(&g.peers[0].h).unwrap() + 500_000;
        while network(&mut g.peers[0]).await.is_ok() {
            assert!(now(&g.peers[0].h).unwrap() < until);
        }
        assert!(g.peers[0].control.check().is_err());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        assert!(g.peers[1].control.check().is_ok());
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn recovery_ticket_entropy_failure_fences_only_failed_viewer() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 2, &[wire::CAPABILITY], false)).await;
        ready(&mut g).await;
        let bytes = request(&g.peers[0]);
        queue(&mut g.peers[0], &bytes).await;
        let p = &mut g.peers[0];
        let until = now(&p.h).unwrap() + 500_000;
        loop {
            assert!(now(&p.h).unwrap() < until);
            let (a, _) = Box::pin(support::both(
                p.shared
                    .as_mut()
                    .unwrap()
                    .drive(Duration::from_millis(1), || Err(()), block),
                p.viewer.drive(Duration::from_millis(1), block),
            ))
            .await;
            if a.is_err() {
                break;
            }
        }
        assert!(p.control.check().is_err());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        assert!(g.peers[1].control.check().is_ok());
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn abandoning_unpolled_shared_recovery_turn_retires_only_that_viewer() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 2, &[wire::CAPABILITY], true)).await;
        ready(&mut g).await;
        admit(&mut g.peers[0]).await;
        drop(
            g.peers[0]
                .shared
                .as_mut()
                .unwrap()
                .drive(Duration::from_millis(1), || Ok(81), block),
        );
        assert!(g.peers[0].control.check().is_err());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        let frame = g.publisher.capture_next().await.unwrap();
        while !g.peers[1].frames.contains(&frame.frame) {
            g.peers[1].turn().await.unwrap();
        }
        cleanup(&mut g, &cx).await;
    });
}

fn presentation(p: &Member) -> Vec<u8> {
    let host = p.shared.as_ref().unwrap();
    let progress = host
        .subscriber
        .as_ref()
        .unwrap()
        .session_progress()
        .unwrap()
        .unwrap();
    let mut view = p.media.binding();
    view.parent = host.session.binding();
    let mut bytes = vec![0; fr_wire::presented::BYTES];
    // Explicit local-visibility fixture. No production code infers it from decode.
    fr_wire::presented::encode(
        fr_wire::presented::Report {
            sequence: 1,
            visible: Some(fr_wire::presented::Sample {
                stamp: fr_wire::presented::Stamp {
                    frame: progress.descriptor.frame,
                    captured_us: progress.descriptor.capture_micros,
                    observed_us: progress.observed_micros,
                    source: progress.observation,
                },
                age_upper_us: now(&p.h).unwrap() - progress.observed_micros,
            }),
        },
        view,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    bytes
}
async fn metrics(p: &mut Member) {
    use fr_wire::receiver_metrics::{self as metrics, Load, Message};
    let old = p.shared.as_ref().unwrap().statistics().feedback_reports;
    let mut view = p.media.binding();
    view.parent = p.shared.as_ref().unwrap().session.binding();
    let until = now(&p.h).unwrap() + 500_000;
    let mut sequence = None;
    while sequence.is_none() {
        assert!(now(&p.h).unwrap() < until);
        let (a, b) = Box::pin(support::both(
            p.shared.as_mut().unwrap().drive(
                Duration::from_millis(1),
                || nonce(&mut p.nonce),
                block,
            ),
            p.viewer.drive(Duration::from_millis(1), |_, bytes| {
                if feedback::is_feedback(bytes) {
                    let Message::Query { sequence: value } = metrics::decode(
                        bytes,
                        view,
                        &ProtocolLimits::ABSOLUTE,
                        InputDirection::HostToViewer,
                        InputDelivery::Reliable,
                    )
                    .unwrap() else {
                        panic!("not a metrics query");
                    };
                    sequence = Some(value);
                    Ok(Disposition::Consumed)
                } else {
                    Ok(Disposition::Blocked)
                }
            }),
        ))
        .await;
        a.unwrap();
        b.unwrap();
    }
    let mut bytes = vec![0; metrics::REPLY_BYTES];
    metrics::encode(
        Message::Reply {
            sequence: sequence.unwrap(),
            load: Load {
                retained_bytes: 0,
                retained_pictures: 0,
                decoding: false,
                work_us: None,
            },
        },
        view,
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    queue(p, &bytes).await;
    while p.shared.as_ref().unwrap().statistics().feedback_reports == old {
        assert!(now(&p.h).unwrap() < until);
        network(p).await.unwrap();
    }
}

#[test]
fn recovered_session_requires_new_generation_visibility_and_metrics_without_losing_totals() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(
            &rt,
            1,
            &[
                wire::CAPABILITY,
                fr_wire::presented::CAPABILITY,
                fr_wire::receiver_metrics::CAPABILITY,
            ],
            true,
        ))
        .await;
        ready(&mut g).await;
        metrics(&mut g.peers[0]).await;
        let bytes = presentation(&g.peers[0]);
        queue(&mut g.peers[0], &bytes).await;
        let until = now(&g.peers[0].h).unwrap() + 250_000;
        while g.peers[0]
            .shared
            .as_ref()
            .unwrap()
            .statistics()
            .presentation_reports
            == 0
        {
            assert!(now(&g.peers[0].h).unwrap() < until);
            network(&mut g.peers[0]).await.unwrap();
        }
        assert!(g.peers[0].control.view_ready().unwrap());
        admit(&mut g.peers[0]).await;
        assert!(!g.peers[0].control.view_ready().unwrap());
        assert!(g.peers[0].shared.as_ref().unwrap().presentation.is_none());
        assert!(g.peers[0].shared.as_ref().unwrap().feedback.is_none());
        queue(&mut g.peers[0], &bytes).await;
        for _ in 0..8 {
            network(&mut g.peers[0]).await.unwrap();
        }
        assert!(!g.peers[0].control.view_ready().unwrap());
        let p = g.peers.pop().unwrap();
        g.peers.push(Box::pin(replace(p)).await);
        g.publisher.capture_next().await.unwrap();
        ready(&mut g).await;
        assert!(!g.peers[0].control.view_ready().unwrap());
        let before = g.peers[0].shared.as_ref().unwrap().statistics();
        assert_eq!(
            (before.presentation_reports, before.feedback_reports),
            (1, 1)
        );
        metrics(&mut g.peers[0]).await;
        let fresh = presentation(&g.peers[0]);
        queue(&mut g.peers[0], &fresh).await;
        while g.peers[0]
            .shared
            .as_ref()
            .unwrap()
            .statistics()
            .presentation_reports
            == 1
        {
            assert!(now(&g.peers[0].h).unwrap() < until + 1_000_000);
            network(&mut g.peers[0]).await.unwrap();
        }
        let after = g.peers[0].shared.as_ref().unwrap().statistics();
        assert_eq!((after.presentation_reports, after.feedback_reports), (2, 2));
        assert_eq!(after.recovered_streams, 1);
        assert!(g.peers[0].control.view_ready().unwrap());
        assert!(g.peers[0].control.check_control().is_err());
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn source_revoked_by_nonce_supplier_prevents_recovery_admission_and_all_further_writes() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 2, &[wire::CAPABILITY], false)).await;
        ready(&mut g).await;
        let bytes = request(&g.peers[0]);
        queue(&mut g.peers[0], &bytes).await;
        let mut calls = 0;
        let p = &mut g.peers[0];
        let until = now(&p.h).unwrap() + 500_000;
        loop {
            assert!(now(&p.h).unwrap() < until);
            let (a, _) = Box::pin(support::both(
                p.shared.as_mut().unwrap().drive(
                    Duration::from_millis(1),
                    || {
                        calls += 1;
                        g.owner.revoke();
                        Ok(81)
                    },
                    block,
                ),
                p.viewer.drive(Duration::from_millis(1), block),
            ))
            .await;
            if a.is_err() {
                break;
            }
        }
        assert_eq!(calls, 1);
        assert_eq!(p.shared.as_ref().unwrap().statistics().recovered_streams, 0);
        assert!(p.control.check().is_err());
        assert!(g.peers[1].turn().await.is_err());
        assert_eq!(g.publisher.physical_usage(), BudgetUsage::default());
        cleanup(&mut g, &cx).await;
    });
}
