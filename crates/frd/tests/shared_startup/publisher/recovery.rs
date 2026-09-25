//! Real lost datagram, original TLS/UDP peers and supervised source/decoder IPC.
//! The codec child is synthetic; these are ownership/protocol, not HEVC gates.
use super::*;
use fr_media::delivery::DeliveryError;
use fr_wire::{Channel, attachment::Ticket, recovery_request as wire};
use frd::media::{decoder_startup::ViewerRecovery, shared_publisher::RecoveryState};

fn request(peer: &Peer) -> Vec<u8> {
    let mut view = peer.media.binding();
    view.parent = parent(peer.link.session);
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
fn admit(peer: &mut Peer) {
    let bytes = request(peer);
    assert!(
        peer.subscriber
            .as_mut()
            .unwrap()
            .request_recovery(
                &peer.link.h,
                peer.link.hr,
                parent(peer.link.session),
                Route::Stream(peer.link.hr.inbound),
                &bytes
            )
            .unwrap()
    );
}
fn tickets() -> [Ticket; 3] {
    [Ticket(8001), Ticket(8002), Ticket(8003)]
}

#[test]
fn shared_loss_recovers_same_sender_and_decoder_while_healthy_viewer_keeps_its_generation() {
    let rt = runtime();
    rt.block_on(recover_loss(&rt, true));
}
#[test]
fn sole_remaining_subscriber_recovers_without_losing_shared_source_ownership() {
    let rt = runtime();
    rt.block_on(recover_loss(&rt, false));
}
#[allow(clippy::too_many_lines)] // One loss-to-decode scenario keeps the same actual owners.
async fn recover_loss(rt: &Runtime, keep_healthy: bool) {
    let cx = Cx::current().unwrap();
    let mut cohort = Box::pin(joined_with_recovery(rt, true, false, true)).await;
    let worker = cohort.publisher.worker_id();
    let healthy_view = cohort.peers[1].media.binding();
    let mut failed = cohort.peers.remove(0);
    if !keep_healthy {
        let mut departed = cohort.peers.remove(0);
        drop(departed.subscriber.take());
        reap(&mut departed.presenter, &cx).await;
    }
    let recipients = if keep_healthy { 2 } else { 1 };
    let decoder_pid = failed.presenter.worker_id();
    let mut reporter = failed
        .media
        .recovery_receiver(
            &failed.link.c,
            failed.link.cr,
            parent(failed.link.session),
            &failed.receiver,
        )
        .unwrap();
    let frame = cohort.publisher.capture_next().await.unwrap();
    assert_eq!(frame.frame, 1);
    if keep_healthy {
        Box::pin(deliver(&mut cohort.peers[0], &cx, 1)).await;
    }
    // Consume actual frame announcement but lose every video datagram.
    let until = clock(&cx) + 1_000_000;
    loop {
        assert!(clock(&cx) < until);
        let sent = failed.service(&cx, 8).unwrap();
        failed.link.drive(&cx).await;
        let mut progress = false;
        failed
            .media
            .receive_ready(
                &cx,
                &mut failed.link.c,
                || true,
                |channel, bytes| {
                    if channel != Channel::Video {
                        failed.receiver.receive(channel, bytes, clock(&cx)).unwrap();
                        progress = true;
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if progress && !sent.pending {
            break;
        }
    }
    // Flush admitted native writes before the silence interval. Otherwise this
    // fixture tests an expired unsent QUIC record rather than a lost datagram.
    for _ in 0..8 {
        failed.service(&cx, 8).unwrap();
        failed.link.drive(&cx).await;
        failed
            .media
            .receive_ready(
                &cx,
                &mut failed.link.c,
                || true,
                |channel, bytes| {
                    if channel != Channel::Video {
                        failed.receiver.receive(channel, bytes, clock(&cx)).unwrap();
                    }
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(260)).await;
    assert_eq!(
        failed.receiver.tick(clock(&cx)),
        Err(DeliveryError::ReferenceExpired)
    );
    loop {
        reporter
            .service(&cx, &mut failed.link.c, &mut failed.receiver, || true)
            .unwrap();
        failed.link.drive(&cx).await;
        if failed
            .subscriber
            .as_mut()
            .unwrap()
            .dispatch_recovery(
                &mut failed.link.h,
                failed.link.hr,
                parent(failed.link.session),
            )
            .unwrap()
        {
            break;
        }
    }
    let Peer {
        mut link,
        media,
        mut presenter,
        mut receiver,
        control,
        subscriber,
    } = failed;
    let mut subscriber = subscriber.unwrap();
    let original_until = subscriber.recovery_deadline(&link.h).unwrap().unwrap();
    assert_eq!(
        subscriber.recovery_state(&link.h).unwrap(),
        RecoveryState::NeedsTickets
    );
    assert_eq!(cohort.publisher.tick().unwrap(), recipients);
    assert_eq!(subscriber.service(&cx, &mut link.h, 8).unwrap().accepted, 0);
    subscriber
        .advance_recovery(&cx, &mut link.h, Some(tickets()))
        .unwrap();
    let mut replacement = media
        .begin_replacement(
            &cx,
            &mut link.c,
            link.cr,
            parent(link.session),
            reporter.next_deadline().unwrap(),
            None,
            || true,
        )
        .unwrap();
    loop {
        assert!(clock(&cx) < original_until);
        if subscriber.recovery_state(&link.h).unwrap() == RecoveryState::Attaching {
            subscriber.advance_recovery(&cx, &mut link.h, None).unwrap();
        }
        replacement.advance(&cx, &mut link.c, || true).unwrap();
        if replacement.is_complete()
            && subscriber.recovery_state(&link.h).unwrap() == RecoveryState::DecoderStartup
        {
            break;
        }
        link.drive(&cx).await;
    }
    let media = replacement.finish(&cx, &mut link.c).unwrap();
    assert_eq!(
        subscriber.recovery_deadline(&link.h).unwrap(),
        Some(original_until)
    );
    let recovered = cohort.publisher.capture_next().await.unwrap();
    assert_eq!(
        (recovered.frame, recovered.delivered, recovered.refused),
        (2, recipients, 0)
    );
    if keep_healthy {
        Box::pin(deliver(&mut cohort.peers[0], &cx, 2)).await;
        assert_eq!(cohort.peers[0].media.binding(), healthy_view);
    }
    let bytes = loop {
        assert!(clock(&cx) < original_until);
        subscriber.service(&cx, &mut link.h, 8).unwrap();
        link.drive(&cx).await;
        let mut bytes = None;
        link.c
            .receive_ready(
                &cx,
                || true,
                |r| matches!(r, Route::Stream(s) if s.messages == Messages::MediaConfiguration),
                |_, b| {
                    bytes = Some(b.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(bytes) = bytes {
            break bytes;
        }
    };
    let mut startup = ViewerRecovery::prepare(
        cx.clone(),
        &link.c,
        &media,
        &bytes,
        reporter.next_deadline().unwrap(),
        &mut reporter,
        &mut presenter,
        &mut receiver,
    )
    .unwrap();
    let mut decoded = false;
    while !startup.is_complete() || !subscriber.startup_complete(&link.h).unwrap() {
        assert!(clock(&cx) < original_until);
        if startup.pending_acknowledgement() {
            startup.transmit(&mut link.c).unwrap();
        }
        subscriber.service(&cx, &mut link.h, 8).unwrap();
        link.drive(&cx).await;
        media
            .receive_ready(
                &cx,
                &mut link.c,
                || true,
                |ch, bytes| {
                    startup.receive_media(ch, bytes).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if !decoded && let Some(receipt) = startup.present_first().await.unwrap() {
            assert_eq!(receipt.frame.as_raw(), 2);
            decoded = true;
        }
    }
    startup.finish(&link.c, &media).unwrap();
    assert!(decoded);
    assert_eq!(presenter.worker_id(), decoder_pid);
    assert_eq!(
        subscriber.recovery_state(&link.h).unwrap(),
        RecoveryState::Receiving
    );
    assert_eq!(subscriber.recovery_deadline(&link.h).unwrap(), None);
    assert_eq!(
        media.binding().recovery,
        healthy_view.recovery.next().unwrap()
    );
    assert!(!control.view_ready().unwrap());
    cohort.peers.push(Peer {
        link,
        media,
        presenter,
        receiver,
        control,
        subscriber: Some(subscriber),
    });
    assert_eq!(cohort.publisher.capture_next().await.unwrap().frame, 3);
    for peer in &mut cohort.peers {
        Box::pin(deliver(peer, &cx, 3)).await;
    }
    assert_eq!(cohort.publisher.worker_id(), worker);
    stop_cohort(&mut cohort, &cx).await;
}

#[test]
fn duplicate_failure_does_not_extend_deadline_or_stop_healthy_capture() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined_with_recovery(&rt, true, false, true)).await;
        admit(&mut cohort.peers[0]);
        let p = &mut cohort.peers[0];
        let bytes = request(p);
        let original = p
            .subscriber
            .as_mut()
            .unwrap()
            .recovery_deadline(&p.link.h)
            .unwrap();
        assert!(
            !p.subscriber
                .as_mut()
                .unwrap()
                .request_recovery(
                    &p.link.h,
                    p.link.hr,
                    parent(p.link.session),
                    Route::Stream(p.link.hr.inbound),
                    &bytes
                )
                .unwrap()
        );
        assert_eq!(
            p.subscriber
                .as_mut()
                .unwrap()
                .recovery_deadline(&p.link.h)
                .unwrap(),
            original
        );
        let next = cohort.publisher.capture_next().await.unwrap();
        assert_eq!((next.delivered, next.refused), (1, 0));
        Box::pin(deliver(&mut cohort.peers[1], &cx, next.frame)).await;
        assert_eq!(cohort.peers[0].service(&cx, 8).unwrap().accepted, 0);
        cohort.owner.check().unwrap();
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn foreign_connection_cannot_fence_a_shared_viewer_with_equal_numeric_routes() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined_with_recovery(&rt, true, false, true)).await;
        let (a, b) = cohort.peers.split_at_mut(1);
        let bytes = request(&a[0]);
        assert_eq!(
            a[0].subscriber.as_mut().unwrap().request_recovery(
                &b[0].link.h,
                a[0].link.hr,
                parent(a[0].link.session),
                Route::Stream(a[0].link.hr.inbound),
                &bytes
            ),
            Err(PublishError::ForeignConnection)
        );
        assert_eq!(cohort.publisher.capture_next().await.unwrap().delivered, 2);
        for peer in &mut cohort.peers {
            Box::pin(deliver(peer, &cx, 1)).await;
        }
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn invalid_replacement_tickets_retire_only_the_failed_subscriber() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined_with_recovery(&rt, true, false, true)).await;
        admit(&mut cohort.peers[0]);
        let p = &mut cohort.peers[0];
        assert!(
            p.subscriber
                .as_mut()
                .unwrap()
                .advance_recovery(&cx, &mut p.link.h, Some([Ticket(1); 3]))
                .is_err()
        );
        assert!(p.control.check().is_err());
        assert_eq!(cohort.publisher.tick().unwrap(), 1);
        let next = cohort.publisher.capture_next().await.unwrap();
        Box::pin(deliver(&mut cohort.peers[1], &cx, next.frame)).await;
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn shared_capture_already_in_flight_finishes_only_for_healthy_viewers() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined_with_recovery(&rt, true, true, true)).await;
        let worker = cohort.publisher.worker_id();
        let next = {
            let mut capture = pin!(cohort.publisher.capture_next());
            poll_fn(|task| {
                assert!(capture.as_mut().poll(task).is_pending());
                Poll::Ready(())
            })
            .await;
            admit(&mut cohort.peers[0]);
            capture.await.unwrap()
        };
        assert_eq!((next.delivered, next.refused), (1, 0));
        assert_eq!(cohort.peers[0].service(&cx, 8).unwrap().accepted, 0);
        Box::pin(deliver(&mut cohort.peers[1], &cx, next.frame)).await;
        assert_eq!(cohort.publisher.worker_id(), worker);
        stop_cohort(&mut cohort, &cx).await;
    });
}
#[test]
fn expired_recovery_cannot_extend_itself_using_healthy_capture_progress() {
    run_shared!(rt, cx, {
        let mut cohort = Box::pin(joined_with_recovery(&rt, true, false, true)).await;
        admit(&mut cohort.peers[0]);
        let failed = &mut cohort.peers[0];
        let until = failed
            .subscriber
            .as_mut()
            .unwrap()
            .recovery_deadline(&failed.link.h)
            .unwrap()
            .unwrap();
        while clock(&cx) < until {
            let next = cohort.publisher.capture_next().await.unwrap();
            assert_eq!(next.delivered, 1);
            Box::pin(deliver(&mut cohort.peers[1], &cx, next.frame)).await;
        }
        assert_eq!(
            cohort.peers[0].service(&cx, 8),
            Err(PublishError::RecoveryExpired)
        );
        assert_eq!(cohort.publisher.tick().unwrap(), 1);
        cohort.peers[1].control.check().unwrap();
        let next = cohort.publisher.capture_next().await.unwrap();
        Box::pin(deliver(&mut cohort.peers[1], &cx, next.frame)).await;
        stop_cohort(&mut cohort, &cx).await;
    });
}
