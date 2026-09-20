//! The normal shared session path, not a manually renewed substitute session.
use super::*;
use crate::media::receiver_feedback::{self as metrics, ViewerFeedback};

#[test]
fn running_session_admits_a_late_viewer_while_original_source_and_renewal_continue() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        ready(&mut g).await;
        let worker = g.publisher.worker_id();
        let mut late = Box::pin(peer(&rt, 14, Role::Observe)).await;
        let queue = g.publisher.join_queue();
        let started = now(&late.h).unwrap();
        let mut reports = 0;
        let source = g
            .publisher
            .serve(Duration::from_millis(50), |_| reports += 1);
        let network = async {
            let session = late.session.take().unwrap();
            late.shared = Some(
                session
                    .join_shared(
                        &queue,
                        late.host_media.take().unwrap(),
                        SendPolicy::default(),
                        Duration::from_secs(2),
                    )
                    .unwrap(),
            );
            assert!(!late.complete());
            assert_eq!(late.frames, [] as [u64; 0]);
            let until = now(&late.h).unwrap() + 1_500_000;
            while !late.complete() {
                assert!(now(&late.h).unwrap() < until);
                let (a, b) = Box::pin(support::both(g.peers[0].turn(), late.turn())).await;
                a.unwrap();
                b.unwrap();
            }
            assert_eq!(late.frames.len(), 1);
            assert!(
                late.frames[0] > 0,
                "a late join cannot replay the initial bootstrap"
            );
            assert!(g.peers[0].frames.contains(&late.frames[0]));
            assert!(!late.control.view_ready().unwrap());
            g.peers[0].shared.as_mut().unwrap().close();
            while now(&late.h).unwrap() < started + 3_200_000 {
                late.turn().await.unwrap();
            }
            assert!(
                late.shared
                    .as_ref()
                    .unwrap()
                    .renewed_until()
                    .unwrap()
                    .as_micros()
                    > started + 3_000_000
            );
            assert!(late.control.check().is_ok());
            assert!(g.peers[0].control.check().is_err());
            assert!(late.shared.as_ref().unwrap().statistics().admitted_records > 10);
            assert_eq!(
                late.frames.len(),
                1,
                "static observations are not dummy video"
            );
            late.shared.as_mut().unwrap().close();
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert_eq!(result, Err(crate::media::shared_publisher::Error::Closed));
        assert!(reports > 10);
        assert_eq!(g.publisher.worker_id(), worker);
        assert!(g.owner.check().is_err());
        late.viewer.close();
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn control_intent_and_foreign_media_cannot_join_an_observation_source() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 1)).await;
        ready(&mut g).await;
        let queue = g.publisher.join_queue();
        let mut control = Box::pin(peer(&rt, 14, Role::RequestControl)).await;
        assert!(matches!(
            control.session.take().unwrap().join_shared(
                &queue,
                control.host_media.take().unwrap(),
                SendPolicy::default(),
                Duration::from_secs(2)
            ),
            Err(Error::Order)
        ));
        assert!(control.control.check().is_err());
        let mut a = Box::pin(peer(&rt, 15, Role::Observe)).await;
        let mut b = Box::pin(peer(&rt, 15, Role::Observe)).await;
        assert!(
            a.session
                .take()
                .unwrap()
                .join_shared(
                    &queue,
                    b.host_media.take().unwrap(),
                    SendPolicy::default(),
                    Duration::from_secs(2)
                )
                .is_err()
        );
        assert!(a.control.check().is_err());
        assert!(b.control.check().is_ok());
        assert!(g.owner.check().is_ok());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        let mut zero = Box::pin(peer(&rt, 16, Role::Observe)).await;
        assert!(
            zero.session
                .take()
                .unwrap()
                .join_shared(
                    &queue,
                    zero.host_media.take().unwrap(),
                    SendPolicy::default(),
                    Duration::ZERO
                )
                .is_err()
        );
        assert!(zero.control.check().is_err());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        cleanup(&mut g, &cx).await;
    });
}

fn feedback(p: &mut Member) -> ViewerFeedback {
    let binding = p.shared.as_ref().unwrap().session.binding();
    let selection = p.shared.as_ref().unwrap().session.selection().clone();
    let setup = metrics::Setup::selected(&selection, binding, p.media.binding())
        .unwrap()
        .unwrap();
    let routes = p.viewer.io().unwrap().1;
    ViewerFeedback::new(
        setup,
        Route::Stream(routes.inbound),
        Route::Stream(routes.outbound),
    )
    .unwrap()
}
fn feedback_io(p: &mut Member, responder: &mut ViewerFeedback) -> usize {
    let at = now(&p.c).unwrap();
    let q = p.viewer.io().unwrap().0;
    let mut count = 0;
    q.receive_ready(
        &p.c,
        || true,
        |_| true,
        |route, bytes| {
            if metrics::is_feedback(bytes) {
                responder.receive(route, bytes, &p.receiver, at).unwrap();
                count += 1;
                Ok(Disposition::Consumed)
            } else {
                Ok(Disposition::Blocked)
            }
        },
    )
    .unwrap();
    responder.service(q, &p.c, at).unwrap();
    count
}

#[test]
fn negotiated_feedback_waits_for_startup_and_does_not_block_observation_renewal() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(
            &rt,
            1,
            &[fr_wire::receiver_metrics::CAPABILITY],
            false,
        ))
        .await;
        let p = &mut g.peers[0];
        let mut responder = feedback(p);
        // Do not configure the decoder. Metadata queries must not consume the
        // same single critical record slot as configuration and challenge work.
        for _ in 0..5 {
            let (host_result, viewer_result) = Box::pin(support::both(
                p.shared.as_mut().unwrap().drive(
                    Duration::from_millis(1),
                    || nonce(&mut p.nonce),
                    block,
                ),
                p.viewer.drive(Duration::from_millis(1), block),
            ))
            .await;
            host_result.unwrap();
            viewer_result.unwrap();
            assert_eq!(feedback_io(p, &mut responder), 0);
        }
        ready(&mut g).await;
        let start = now(&g.peers[0].h).unwrap();
        let source = g.publisher.serve(Duration::from_millis(50), |_| {});
        let network = async {
            let p = &mut g.peers[0];
            let mut queries = 0;
            while now(&p.h).unwrap() < start + 3_200_000 {
                p.turn().await.unwrap();
                queries += feedback_io(p, &mut responder);
            }
            assert!(queries >= 2);
            assert!(responder.sent >= 2);
            assert!(p.shared.as_ref().unwrap().statistics().feedback_reports >= 2);
            assert!(
                p.shared
                    .as_ref()
                    .unwrap()
                    .renewed_until()
                    .unwrap()
                    .as_micros()
                    > start + 3_000_000
            );
            assert!(
                !p.control.view_ready().unwrap(),
                "advisory load cannot grant input readiness"
            );
            p.shared.as_mut().unwrap().close();
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert_eq!(result, Err(crate::media::shared_publisher::Error::Closed));
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn missing_reference_is_repaired_through_original_shared_session_dispatch() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group_with_capabilities(&rt, 2, &[], true)).await;
        ready(&mut g).await;
        let worker = g.publisher.worker_id();
        let frame = g.publisher.capture_next().await.unwrap().frame;
        assert_eq!(frame, 1);
        let start = now(&g.peers[0].h).unwrap();
        // Drop actual viewer-side video records while preserving reliable source
        // progress. Neither this loss nor the repair changes sender generation.
        while now(&g.peers[0].h).unwrap() < start + 30_000 {
            g.peers[0].turn_with_loss(true).await.unwrap();
            g.peers[1].turn().await.unwrap();
        }
        assert_eq!(g.peers[0].frames, [0]);
        assert_eq!(g.peers[1].frames, [0, 1]);
        let p = &mut g.peers[0];
        let mut bytes = [0; 1150];
        let offer = p
            .receiver
            .repair_offer(now(&p.c).unwrap(), &mut bytes)
            .unwrap()
            .unwrap();
        assert_eq!(offer.frame, frame);
        let route = p.media.repair_stream(p.viewer.io().unwrap().0).unwrap();
        loop {
            assert!(now(&p.c).unwrap() < offer.reference_deadline_us);
            let q = p.viewer.io().unwrap().0;
            match q.send(
                &p.c,
                Route::Stream(route),
                &bytes[..offer.bytes],
                offer.reference_deadline_us,
                || true,
            ) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => {
                    p.turn().await.unwrap();
                }
                result => panic!("repair refused: {result:?}"),
            }
        }
        while !p.frames.contains(&1) {
            assert!(now(&p.c).unwrap() < offer.reference_deadline_us);
            p.turn().await.unwrap();
        }
        assert_eq!(p.shared.as_ref().unwrap().statistics().repair_requests, 1);
        assert_eq!(p.frames, [0, 1]);
        assert_eq!(
            g.peers[1]
                .shared
                .as_ref()
                .unwrap()
                .statistics()
                .repair_requests,
            0
        );
        let next = g.publisher.capture_next().await.unwrap();
        assert_eq!(next.frame, 2);
        let until = now(&cx).unwrap() + 200_000;
        while g.peers.iter().any(|peer| !peer.frames.contains(&2)) {
            assert!(now(&cx).unwrap() < until);
            for p in &mut g.peers {
                p.turn().await.unwrap();
            }
        }
        assert_eq!(g.publisher.worker_id(), worker);
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn unnegotiated_feedback_refuses_only_the_offending_shared_session() {
    let rt = support::runtime();
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 2)).await;
        ready(&mut g).await;
        let p = &mut g.peers[0];
        let parent = p.shared.as_ref().unwrap().session.binding();
        let mut view = p.media.binding();
        view.parent = parent;
        let mut bytes = [0; fr_wire::receiver_metrics::REPLY_BYTES];
        let n = fr_wire::receiver_metrics::encode(
            fr_wire::receiver_metrics::Message::Reply {
                sequence: 1,
                load: fr_wire::receiver_metrics::Load {
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
        let until = now(&p.c).unwrap() + 500_000;
        loop {
            assert!(now(&cx).unwrap() < until);
            let (q, routes) = p.viewer.io().unwrap();
            match q.send(
                &p.c,
                Route::Stream(routes.outbound),
                &bytes[..n],
                until,
                || true,
            ) {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => {
                    p.turn().await.unwrap();
                }
                result => panic!("injected report refused before dispatch: {result:?}"),
            }
        }
        loop {
            assert!(now(&cx).unwrap() < until);
            if p.turn().await.is_err() {
                break;
            }
        }
        assert!(p.control.check().is_err());
        assert_eq!(g.publisher.tick().unwrap(), 1);
        assert!(g.owner.check().is_ok());
        g.peers[1].turn().await.unwrap();
        assert!(g.peers[1].control.check().is_ok());
        cleanup(&mut g, &cx).await;
    });
}
