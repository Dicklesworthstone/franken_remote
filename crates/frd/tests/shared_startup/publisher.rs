//! Production shared publisher with completed TLS/UDP/native decoder handoffs.
//! Changed pixels and native decode are synthetic fixtures, not HEVC qualification.
use super::*;
use asupersync::runtime::Runtime;
use frd::{
    media::shared_publisher::{Error as PublishError, Publisher, Subscriber},
    media_quic::NegotiatedMedia,
};
use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

fn sendable<T: Send>(_: &T) {}

struct Peer {
    link: Link,
    media: NegotiatedMedia,
    presenter: Presenter,
    receiver: ReceivePipeline,
    control: ObservationControl,
    subscriber: Option<Subscriber>,
}
impl Peer {
    fn service(
        &mut self,
        cx: &Cx,
        maximum: usize,
    ) -> Result<frd::media::shared_publisher::SendReport, PublishError> {
        self.subscriber
            .as_mut()
            .unwrap()
            .service(cx, &mut self.link.h, maximum)
    }
}
struct Cohort {
    publisher: Publisher,
    peers: Vec<Peer>,
    owner: ObservationControl,
}
async fn joined(rt: &Runtime, changing: bool, delayed: bool) -> Box<Cohort> {
    Box::pin(joined_with_recovery(rt, changing, delayed, false)).await
}
async fn joined_with_recovery(
    rt: &Runtime,
    changing: bool,
    delayed: bool,
    recovery: bool,
) -> Box<Cohort> {
    let cx = Cx::current().unwrap();
    let (owner, a, b) = (gate(rt, 1), gate(rt, 13), gate(rt, 14));
    let mut al = Link::new(&cx, 13).await;
    let mut bl = Link::new(&cx, 14).await;
    if recovery {
        for link in [&mut al, &mut bl] {
            link.selection
                .capabilities
                .push(fr_wire::negotiation::Capability {
                    name: fr_wire::recovery_request::CAPABILITY.into(),
                    version: 1,
                    required: true,
                });
            link.selection
                .capabilities
                .sort_by(|a, b| a.name.cmp(&b.name));
        }
    }
    let am = al.media(&cx).await;
    let bm = bl.media(&cx).await;
    let mut source = source_variant(&owner, true, changing, delayed).await;
    let pool = pool();
    let initial = source
        .prepare_shared_capture(&owner, &pool)
        .unwrap()
        .capture_if_changed(true)
        .await
        .unwrap();
    let mut ah = Host::new_shared(
        a.clone(),
        &al.h,
        am.host
            .decoder_setup(&al.h, Duration::from_secs(2))
            .unwrap(),
        configuration(),
        initial.clone(),
    )
    .unwrap();
    let mut bh = Host::new_shared(
        b.clone(),
        &bl.h,
        bm.host
            .decoder_setup(&bl.h, Duration::from_secs(2))
            .unwrap(),
        configuration(),
        initial.clone(),
    )
    .unwrap();
    let (av, au) = configured(&mut al, &am, &mut ah, &cx).await;
    let (sa, pa, ra) = Box::pin(finish(&mut al, &am, &mut ah, av, &au, &a, &cx)).await;
    let (bv, bu) = configured(&mut bl, &bm, &mut bh, &cx).await;
    let (sb, pb, rb) = Box::pin(finish(&mut bl, &bm, &mut bh, bv, &bu, &b, &cx)).await;
    let mut publisher = Publisher::new(source, owner.clone(), pool, &initial).unwrap();
    let ha = publisher.admit(ah, sa, am.host, &al.h).unwrap();
    let hb = publisher.admit(bh, sb, bm.host, &bl.h).unwrap();
    drop((initial, au, bu));
    assert_eq!(publisher.tick().unwrap(), 2);
    Box::new(Cohort {
        publisher,
        owner,
        peers: vec![
            Peer {
                link: al,
                media: am.viewer,
                presenter: pa,
                receiver: ra,
                control: a,
                subscriber: Some(ha),
            },
            Peer {
                link: bl,
                media: bm.viewer,
                presenter: pb,
                receiver: rb,
                control: b,
                subscriber: Some(hb),
            },
        ],
    })
}
async fn deliver(peer: &mut Peer, cx: &Cx, frame: u64) {
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(
            clock(cx) < until,
            "new frame did not reach original decoder"
        );
        // A per-connection bounded turn, not a queue shared by viewers.
        let report = peer
            .subscriber
            .as_mut()
            .unwrap()
            .service(cx, &mut peer.link.h, 1)
            .unwrap();
        assert!(report.accepted <= 1);
        peer.link.drive(cx).await;
        peer.media
            .receive_ready(
                cx,
                &mut peer.link.c,
                || true,
                |channel, bytes| {
                    peer.receiver.receive(channel, bytes, clock(cx)).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(receipt) = peer
            .presenter
            .present_next(cx, &mut peer.receiver)
            .await
            .unwrap()
        {
            assert_eq!(receipt.frame.as_raw(), frame);
            return;
        }
    }
}
async fn stop_cohort(cohort: &mut Cohort, cx: &Cx) {
    for peer in &mut cohort.peers {
        drop(peer.subscriber.take());
        peer.receiver.close();
    }
    cohort
        .publisher
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
    for peer in &mut cohort.peers {
        reap(&mut peer.presenter, cx).await;
    }
    assert_eq!(cohort.publisher.physical_usage(), BudgetUsage::default());
}

#[test]
fn completed_viewers_share_continuous_native_capture_and_survive_independent_departure() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let capture_pid = cohort.publisher.worker_id();
        let decoders = [
            cohort.peers[0].presenter.worker_id(),
            cohort.peers[1].presenter.worker_id(),
        ];
        for frame in 1..=3 {
            let result = cohort.publisher.capture_next().await.unwrap();
            assert_eq!(
                (
                    result.frame,
                    result.delivered,
                    result.refused,
                    result.unchanged
                ),
                (frame, 2, 0, false)
            );
            for peer in &mut cohort.peers {
                Box::pin(deliver(peer, &cx, frame)).await;
            }
            // Capture-anchored eviction can legitimately retire earlier dependent
            // frames while actual network/child operations run. Two viewers must
            // never retain two physical copies of any one produced picture.
            assert!(
                (1..=usize::try_from(frame + 1).unwrap())
                    .contains(&cohort.publisher.physical_usage().pictures)
            );
        }
        drop(cohort.peers[0].subscriber.take());
        assert!(cohort.peers[0].control.check().is_err());
        cohort.peers[1].control.check().unwrap();
        cohort.owner.check().unwrap();
        assert_eq!(cohort.publisher.tick().unwrap(), 1);
        let result = cohort.publisher.capture_next().await.unwrap();
        assert_eq!((result.frame, result.delivered), (4, 1));
        Box::pin(deliver(&mut cohort.peers[1], &cx, 4)).await;
        assert_eq!(cohort.publisher.worker_id(), capture_pid);
        assert_eq!(
            [
                cohort.peers[0].presenter.worker_id(),
                cohort.peers[1].presenter.worker_id()
            ],
            decoders
        );
        assert!(!cohort.peers[1].control.view_ready().unwrap());
        stop_cohort(&mut cohort, &cx).await;
        assert!(cohort.owner.check().is_err());
        assert!(cohort.publisher.capture_next().await.is_err());
    });
}

#[test]
fn slow_viewer_is_refused_before_a_healthy_viewers_next_reference_is_encoded() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        cohort.publisher.capture_next().await.unwrap();
        Box::pin(deliver(&mut cohort.peers[0], &cx, 1)).await;
        // Peer1 has not drained its first dependent picture. No hidden FIFO or
        // skipped reference may be created for it by a second changed capture.
        let next = cohort.publisher.capture_next().await.unwrap();
        assert_eq!((next.frame, next.delivered, next.refused), (2, 1, 1));
        assert!(cohort.peers[1].control.check().is_err());
        assert_eq!(
            cohort.peers[1].service(&cx, 1),
            Err(PublishError::SlowSubscriber)
        );
        cohort.owner.check().unwrap();
        cohort.peers[0].control.check().unwrap();
        Box::pin(deliver(&mut cohort.peers[0], &cx, 2)).await;
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn all_blocked_viewers_pause_raw_capture_without_skipping_reference_identity() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let first = cohort.publisher.capture_next().await.unwrap();
        assert_eq!(first.frame, 1);
        let held = cohort.publisher.physical_usage();
        for _ in 0..8 {
            assert_eq!(
                cohort.publisher.capture_next().await,
                Err(PublishError::Media(frd::media::Error::Backpressure))
            );
        }
        assert_eq!(cohort.publisher.physical_usage(), held);
        for peer in &mut cohort.peers {
            peer.control.check().unwrap();
            Box::pin(deliver(peer, &cx, 1)).await;
        }
        let next = cohort.publisher.capture_next().await.unwrap();
        assert_eq!(next.frame, 2);
        for peer in &mut cohort.peers {
            Box::pin(deliver(peer, &cx, 2)).await;
        }
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn cancelling_native_publication_fences_every_viewer_before_reaping_original_worker() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, true)).await;
        let pid = cohort.publisher.worker_id();
        {
            let mut capture = pin!(cohort.publisher.capture_next());
            poll_fn(|task| match capture.as_mut().poll(task) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(v) => panic!("expected pending IPC: {v:?}"),
            })
            .await;
            // These connection handles remain usable while the shared source is
            // exclusively borrowed by an actual native exchange.
            for peer in &mut cohort.peers {
                peer.subscriber
                    .as_mut()
                    .unwrap()
                    .service(&cx, &mut peer.link.h, 1)
                    .unwrap();
            }
        }
        for peer in &mut cohort.peers {
            assert!(peer.control.check().is_err());
            assert!(
                peer.subscriber
                    .as_mut()
                    .unwrap()
                    .service(&cx, &mut peer.link.h, 1)
                    .is_err()
            );
        }
        assert!(cohort.owner.check().is_err());
        assert_eq!(cohort.publisher.worker_id(), pid);
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn source_revocation_fences_all_queued_outputs_even_without_another_capture_turn() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        cohort.publisher.capture_next().await.unwrap();
        cohort.owner.revoke();
        assert!(cohort.peers[0].service(&cx, 1).is_err());
        for peer in &cohort.peers {
            assert!(peer.control.check().is_err());
        }
        assert_eq!(cohort.publisher.physical_usage(), BudgetUsage::default());
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn equal_numbered_foreign_connection_and_invalid_turn_budget_do_not_mutate_membership() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let [a, b] = cohort.peers.as_mut_slice() else {
            panic!("two admitted peers");
        };
        assert_eq!(
            a.subscriber
                .as_mut()
                .unwrap()
                .service(&cx, &mut b.link.h, 1),
            Err(PublishError::ForeignConnection)
        );
        assert_eq!(
            a.subscriber
                .as_mut()
                .unwrap()
                .service(&cx, &mut a.link.h, 0),
            Err(PublishError::InvalidBudget)
        );
        assert_eq!(
            a.subscriber
                .as_mut()
                .unwrap()
                .service(&cx, &mut a.link.h, 65),
            Err(PublishError::InvalidBudget)
        );
        assert!(!a.link.h.is_closed());
        assert!(!b.link.h.is_closed());
        a.control.check().unwrap();
        b.control.check().unwrap();
        assert_eq!(cohort.publisher.tick().unwrap(), 2);
        cohort.publisher.capture_next().await.unwrap();
        for peer in &mut cohort.peers {
            Box::pin(deliver(peer, &cx, 1)).await;
        }
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn static_source_observations_allocate_no_new_retained_picture() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, false, false)).await;
        let initial = cohort.publisher.physical_usage();
        for _ in 0..3 {
            let report = cohort.publisher.capture_next().await.unwrap();
            assert_eq!(
                (report.frame, report.delivered, report.unchanged),
                (0, 2, true)
            );
            for peer in &mut cohort.peers {
                let until = clock(&cx) + 500_000;
                let mut received = false;
                while !received {
                    assert!(clock(&cx) < until, "source observation did not arrive");
                    peer.subscriber
                        .as_mut()
                        .unwrap()
                        .service(&cx, &mut peer.link.h, 1)
                        .unwrap();
                    peer.link.drive(&cx).await;
                    peer.media
                        .receive_ready(
                            &cx,
                            &mut peer.link.c,
                            || true,
                            |channel, bytes| {
                                peer.receiver.receive(channel, bytes, clock(&cx)).unwrap();
                                received = true;
                                Ok(Disposition::Consumed)
                            },
                        )
                        .unwrap();
                }
                assert!(
                    peer.presenter
                        .present_next(&cx, &mut peer.receiver)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            assert_eq!(cohort.publisher.physical_usage(), initial);
        }
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn publisher_rejects_copied_pool_allowance_and_another_native_source() {
    run_shared!(rt, cx, {
        let owner = gate(&rt, 1);
        let mut a = source(&owner, true).await;
        let p = pool();
        let initial = a
            .prepare_shared_capture(&owner, &p)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let b = source(&owner, true).await;
        assert!(matches!(
            Publisher::new(b, owner.clone(), p.clone(), &initial),
            Err(PublishError::Media(_))
        ));
        assert!(matches!(
            Publisher::new(a, owner.clone(), pool(), &initial),
            Err(PublishError::Media(_))
        ));
        // Rejected ownership transfers do not grant or revoke unrelated consent.
        owner.check().unwrap();
        drop(initial);
        assert_eq!(p.usage(), BudgetUsage::default());
        let _ = cx;
    });
}

#[test]
fn last_subscriber_departure_interrupts_pending_capture_and_keeps_the_child_collectable() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, true)).await;
        let pid = cohort.publisher.worker_id();
        {
            let mut capture = pin!(cohort.publisher.capture_next());
            poll_fn(|task| match capture.as_mut().poll(task) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(v) => panic!("expected pending IPC: {v:?}"),
            })
            .await;
            for peer in &mut cohort.peers {
                drop(peer.subscriber.take());
                assert!(peer.control.check().is_err());
            }
            assert!(cohort.owner.check().is_err());
            assert!(capture.await.is_err());
        }
        assert_eq!(cohort.publisher.worker_id(), pid);
        assert!(cohort.publisher.tick().is_err());
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn unpolled_publication_cancellation_is_terminal_before_any_native_work() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let pid = cohort.publisher.worker_id();
        let operation = cohort.publisher.capture_next();
        sendable(&operation);
        drop(operation);
        assert!(cohort.owner.check().is_err());
        assert!(cohort.peers.iter().all(|p| p.control.check().is_err()));
        assert_eq!(cohort.publisher.physical_usage(), BudgetUsage::default());
        assert_eq!(cohort.publisher.worker_id(), pid);
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn native_completion_does_not_restore_a_viewer_revoked_during_capture() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined(&rt, true, true)).await;
        let result = {
            let mut capture = pin!(cohort.publisher.capture_next());
            poll_fn(|task| match capture.as_mut().poll(task) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(v) => panic!("expected pending IPC: {v:?}"),
            })
            .await;
            cohort.peers[0].control.revoke();
            capture.await.unwrap()
        };
        assert_eq!((result.frame, result.delivered), (1, 1));
        assert!(cohort.peers[0].service(&cx, 1).is_err());
        cohort.owner.check().unwrap();
        cohort.peers[1].control.check().unwrap();
        Box::pin(deliver(&mut cohort.peers[1], &cx, 1)).await;
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn an_unfinished_decoder_cannot_enter_shared_publication() {
    run_shared!(rt, cx, {
        let owner = gate(&rt, 1);
        let viewer = gate(&rt, 13);
        let mut link = Link::new(&cx, 13).await;
        let media = link.media(&cx).await;
        let mut source = source(&owner, true).await;
        let pool = pool();
        let initial = source
            .prepare_shared_capture(&owner, &pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        let host = Host::new_shared(
            viewer.clone(),
            &link.h,
            media
                .host
                .decoder_setup(&link.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            initial.clone(),
        )
        .unwrap();
        let sender = media
            .host
            .sender(&link.h, viewer.clone(), SendPolicy::default())
            .unwrap();
        let mut publisher = Publisher::new(source, owner.clone(), pool, &initial).unwrap();
        drop(initial);
        assert!(matches!(
            publisher.admit(host, sender, media.host, &link.h),
            Err(PublishError::Startup(Error::WrongState))
        ));
        assert_eq!(publisher.tick().unwrap(), 0);
        assert_eq!(
            publisher.capture_next().await,
            Err(PublishError::NoSubscribers)
        );
        owner.check().unwrap();
        viewer.check().unwrap();
        publisher
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

#[path = "publisher/service.rs"]
mod service;

#[path = "publisher/consent.rs"]
mod consent;

#[path = "publisher/recovery.rs"]
mod recovery;
