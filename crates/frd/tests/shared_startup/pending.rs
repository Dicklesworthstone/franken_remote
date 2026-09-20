//! Real connection/child startup with pending slots, not hardware HEVC evidence.
use super::*;
use asupersync::runtime::Runtime;
use frd::{
    media::shared_publisher::{Error as PublishError, MAX_SUBSCRIBERS, Publisher, Subscriber},
    media_quic::NegotiatedMedia,
};

struct PendingPeer {
    link: Link,
    media: NegotiatedMedia,
    control: ObservationControl,
    reply: quic::StreamRoute,
    deadline: u64,
    subscriber: Option<Subscriber>,
}
struct PendingGroup {
    publisher: Publisher,
    owner: ObservationControl,
    initial: Option<SharedCaptureUpdate>,
    peers: Vec<PendingPeer>,
}
async fn group(rt: &Runtime, count: usize) -> Box<PendingGroup> {
    group_with_timeout(rt, count, Duration::from_secs(2)).await
}
async fn group_with_timeout(
    rt: &Runtime,
    count: usize,
    first_timeout: Duration,
) -> Box<PendingGroup> {
    let cx = Cx::current().unwrap();
    let owner = gate(rt, 1);
    let mut inputs = Vec::new();
    for i in 0..count {
        let session = 13 + u128::try_from(i).unwrap();
        let mut link = Link::new(&cx, session).await;
        let media = link.media(&cx).await;
        inputs.push((gate(rt, session), link, media));
    }
    let mut source = source_variant(&owner, true, true, false).await;
    let pool = pool();
    let initial = source
        .prepare_shared_capture(&owner, &pool)
        .unwrap()
        .capture_if_changed(true)
        .await
        .unwrap();
    let mut publisher = Publisher::new(source, owner.clone(), pool, &initial).unwrap();
    let mut peers = Vec::new();
    for (i, (control, link, media)) in inputs.into_iter().enumerate() {
        let startup = Host::new_shared(
            control.clone(),
            &link.h,
            media
                .host
                .decoder_setup(
                    &link.h,
                    if i == 0 {
                        first_timeout
                    } else {
                        Duration::from_secs(2)
                    },
                )
                .unwrap(),
            configuration(),
            initial.clone(),
        )
        .unwrap();
        let sender = media
            .host
            .sender(&link.h, control.clone(), SendPolicy::default())
            .unwrap();
        let deadline = startup.deadline_us();
        let subscriber = publisher
            .admit_pending(startup, sender, media.host, &link.h)
            .unwrap();
        peers.push(PendingPeer {
            link,
            media: media.viewer,
            control,
            reply: media.reply,
            deadline,
            subscriber: Some(subscriber),
        });
    }
    Box::new(PendingGroup {
        publisher,
        owner,
        initial: Some(initial),
        peers,
    })
}
impl PendingPeer {
    fn service(
        &mut self,
        cx: &Cx,
    ) -> Result<frd::media::shared_publisher::SendReport, PublishError> {
        let report = self
            .subscriber
            .as_mut()
            .unwrap()
            .service(cx, &mut self.link.h, 1)?;
        assert!(
            report.accepted <= 1,
            "configuration and media share one send budget"
        );
        Ok(report)
    }
    fn complete(&mut self) -> Result<bool, PublishError> {
        self.subscriber
            .as_mut()
            .unwrap()
            .startup_complete(&self.link.h)
    }
}
async fn configure(peer: &mut PendingPeer, cx: &Cx) -> Viewer {
    let until = clock(cx) + 1_000_000;
    let bytes = loop {
        assert!(clock(cx) < until);
        peer.service(cx).unwrap();
        peer.link.drive(cx).await;
        let mut configuration = None;
        peer.link
            .c
            .receive_ready(
                cx,
                || true,
                |r| matches!(r, Route::Stream(s) if s.messages == Messages::Exact(0x30)),
                |_, b| {
                    configuration = Some(b.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(bytes) = configuration {
            break bytes;
        }
    };
    assert!(!peer.complete().unwrap());
    // Host is still waiting for Configured: repeated service cannot leak its IDR.
    for _ in 0..3 {
        assert_eq!(peer.service(cx).unwrap().accepted, 0);
        peer.link.drive(cx).await;
        let mut leaked = false;
        peer.media
            .receive_ready(
                cx,
                &mut peer.link.c,
                || true,
                |_, _| {
                    leaked = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        assert!(!leaked, "pixels before actual decoder configuration");
    }
    Viewer::start(
        cx.clone(),
        &peer.link.c,
        peer.media
            .decoder_setup(&peer.link.c, Duration::from_secs(2))
            .unwrap(),
        &bytes,
        decoder(),
        peer.media
            .receiver_config(&peer.link.c, ReceivePolicy::default())
            .unwrap(),
    )
    .await
    .unwrap()
}
async fn decode_without_ack(peer: &mut PendingPeer, viewer: &mut Viewer, cx: &Cx) {
    let until = clock(cx) + 1_000_000;
    while !viewer.transmit(&mut peer.link.c).unwrap() {
        peer.link.drive(cx).await;
    }
    loop {
        assert!(clock(cx) < until);
        peer.link.drive(cx).await;
        peer.service(cx).unwrap();
        peer.media
            .receive_ready(
                cx,
                &mut peer.link.c,
                || true,
                |channel, bytes| {
                    viewer.receive_media(channel, bytes).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if let Some(receipt) = viewer.present_first().await.unwrap() {
            assert_eq!(receipt.frame.as_raw(), 0);
            break;
        }
    }
    assert!(!peer.complete().unwrap());
}
async fn acknowledge(
    peer: &mut PendingPeer,
    mut viewer: Viewer,
    cx: &Cx,
) -> (Presenter, ReceivePipeline) {
    let until = clock(cx) + 1_000_000;
    while !viewer.transmit(&mut peer.link.c).unwrap() {
        peer.link.drive(cx).await;
    }
    while !peer.complete().unwrap() {
        assert!(clock(cx) < until);
        peer.link.drive(cx).await;
        peer.service(cx).unwrap();
    }
    assert!(
        !peer.control.view_ready().unwrap(),
        "decoding does not grant visibility or input"
    );
    viewer.finish().unwrap()
}
async fn while_driving<T>(
    peer: &mut PendingPeer,
    cx: &Cx,
    operation: impl std::future::Future<Output = T>,
) -> T {
    use std::{future::poll_fn, pin::pin, task::Poll};
    let mut operation = pin!(operation);
    let mut result = None;
    loop {
        // Never abandon an in-flight transport turn on native completion.
        let mut network = pin!(peer.link.drive(cx));
        poll_fn(|task| {
            if result.is_none()
                && let Poll::Ready(value) = operation.as_mut().poll(task)
            {
                result = Some(value);
            }
            network.as_mut().poll(task)
        })
        .await;
        if let Some(result) = result.take() {
            return result;
        }
    }
}
async fn receive_frame(
    peer: &mut PendingPeer,
    presenter: &mut Presenter,
    receiver: &mut ReceivePipeline,
    frame: u64,
    cx: &Cx,
) {
    let until = clock(cx) + 1_000_000;
    loop {
        assert!(clock(cx) < until);
        peer.service(cx).unwrap();
        peer.link.drive(cx).await;
        peer.media
            .receive_ready(
                cx,
                &mut peer.link.c,
                || true,
                |channel, bytes| {
                    receiver.receive(channel, bytes, clock(cx)).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        // Native decoding deliberately waits 90 ms. Keep this connection driven
        // through that wait so retained reliable records can be acknowledged;
        // pausing UDP behind the decoder would test a broken parent driver.
        let decoded = while_driving(peer, cx, presenter.present_next(cx, receiver)).await;
        if let Some(receipt) = decoded.unwrap() {
            assert_eq!(receipt.frame.as_raw(), frame);
            break;
        }
    }
}
async fn cleanup(group: &mut PendingGroup, cx: &Cx) {
    for peer in &mut group.peers {
        drop(peer.subscriber.take());
    }
    drop(group.initial.take());
    group
        .publisher
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
}

#[test]
fn pending_viewers_finish_independently_on_one_source_and_only_then_receive_dependents() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, 2)).await;
        let worker = group.publisher.worker_id();
        let usage = group.publisher.physical_usage();
        assert_eq!(usage.pictures, 1);
        assert_eq!(group.publisher.tick().unwrap(), 2);
        // Nobody is configured: there is no hidden encode or skipped reference.
        assert_eq!(
            group.publisher.capture_next().await.unwrap_err(),
            PublishError::Media(frd::media::Error::Backpressure)
        );
        assert_eq!(group.publisher.physical_usage(), usage);
        let mut a = Box::pin(configure(&mut group.peers[0], &cx)).await;
        let mut b = Box::pin(configure(&mut group.peers[1], &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[0], &mut a, &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[1], &mut b, &cx)).await;
        // The next real reference can occupy the original bounded send caches,
        // but neither peer can receive it until its own FirstDecoded arrives.
        let update = group.publisher.capture_next().await.unwrap();
        assert_eq!((update.frame, update.delivered, update.refused), (1, 2, 0));
        for peer in &mut group.peers {
            for _ in 0..3 {
                assert_eq!(peer.service(&cx).unwrap().accepted, 0);
                peer.link.drive(&cx).await;
                let mut delivered = 0;
                peer.media
                    .receive_ready(
                        &cx,
                        &mut peer.link.c,
                        || true,
                        |_, _| {
                            delivered += 1;
                            Ok(Disposition::Consumed)
                        },
                    )
                    .unwrap();
                assert_eq!(delivered, 0, "dependent frame crossed FirstDecoded gate");
            }
        }
        let (mut ap, mut ar) = Box::pin(acknowledge(&mut group.peers[0], a, &cx)).await;
        Box::pin(receive_frame(&mut group.peers[0], &mut ap, &mut ar, 1, &cx)).await;
        assert!(!group.peers[1].complete().unwrap());
        let (mut bp, mut br) = Box::pin(acknowledge(&mut group.peers[1], b, &cx)).await;
        Box::pin(receive_frame(&mut group.peers[1], &mut bp, &mut br, 1, &cx)).await;
        assert_eq!(group.publisher.capture_next().await.unwrap().frame, 2);
        Box::pin(receive_frame(&mut group.peers[0], &mut ap, &mut ar, 2, &cx)).await;
        Box::pin(receive_frame(&mut group.peers[1], &mut bp, &mut br, 2, &cx)).await;
        assert_eq!(group.publisher.worker_id(), worker);
        cleanup(&mut group, &cx).await;
        reap(&mut ap, &cx).await;
        reap(&mut bp, &cx).await;
    });
}

#[test]
fn one_unconfigured_peer_cannot_hold_back_an_already_decoding_viewer() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, 2)).await;
        let mut a = Box::pin(configure(&mut group.peers[0], &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[0], &mut a, &cx)).await;
        let (mut presenter, mut receiver) =
            Box::pin(acknowledge(&mut group.peers[0], a, &cx)).await;
        let next = group.publisher.capture_next().await.unwrap();
        assert_eq!((next.frame, next.delivered, next.refused), (1, 1, 1));
        assert_eq!(
            group.peers[1].service(&cx),
            Err(PublishError::SlowSubscriber)
        );
        assert!(group.peers[1].control.check().is_err());
        group.owner.check().unwrap();
        group.peers[0].control.check().unwrap();
        Box::pin(receive_frame(
            &mut group.peers[0],
            &mut presenter,
            &mut receiver,
            1,
            &cx,
        ))
        .await;
        cleanup(&mut group, &cx).await;
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn pending_last_viewer_drop_ends_source_and_keeps_the_original_child_collectable() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, 2)).await;
        let worker = group.publisher.worker_id();
        drop(group.peers[0].subscriber.take());
        assert_eq!(group.publisher.tick().unwrap(), 1);
        group.owner.check().unwrap();
        assert!(group.peers[0].control.check().is_err());
        drop(group.peers[1].subscriber.take());
        assert!(group.owner.check().is_err());
        assert!(group.publisher.tick().is_err());
        assert_eq!(group.publisher.worker_id(), worker);
        cleanup(&mut group, &cx).await;
    });
}

#[test]
fn foreign_connection_cannot_advance_or_cancel_a_pending_handshake() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, 2)).await;
        let (a, b) = group.peers.split_at_mut(1);
        assert_eq!(
            a[0].subscriber
                .as_mut()
                .unwrap()
                .service(&cx, &mut b[0].link.h, 1),
            Err(PublishError::ForeignConnection)
        );
        assert!(!a[0].complete().unwrap());
        a[0].control.check().unwrap();
        b[0].control.check().unwrap();
        group.owner.check().unwrap();
        let viewer = Box::pin(configure(&mut group.peers[0], &cx)).await;
        let mut viewer = viewer;
        viewer.close();
        viewer
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        cleanup(&mut group, &cx).await;
    });
}

#[test]
fn pending_slots_share_the_hard_cohort_bound_and_rejected_admission_does_not_send() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, MAX_SUBSCRIBERS)).await;
        assert_eq!(group.publisher.tick().unwrap(), MAX_SUBSCRIBERS);
        assert_eq!(group.publisher.physical_usage().pictures, 1);
        let mut ninth = Link::new(&cx, 100).await;
        let media = ninth.media(&cx).await;
        let control = gate(&rt, 100);
        let startup = Host::new_shared(
            control.clone(),
            &ninth.h,
            media
                .host
                .decoder_setup(&ninth.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            group.initial.as_ref().unwrap().clone(),
        )
        .unwrap();
        let sender = media
            .host
            .sender(&ninth.h, control.clone(), SendPolicy::default())
            .unwrap();
        let before = ninth.h.usage();
        assert!(matches!(
            group
                .publisher
                .admit_pending(startup, sender, media.host, &ninth.h),
            Err(PublishError::Full)
        ));
        assert_eq!(
            ninth.h.usage().retained_send_records,
            before.retained_send_records
        );
        assert_eq!(group.publisher.tick().unwrap(), MAX_SUBSCRIBERS);
        for peer in &group.peers {
            peer.control.check().unwrap();
        }
        control.check().unwrap();
        group.owner.check().unwrap();
        cleanup(&mut group, &cx).await;
        assert_eq!(group.publisher.physical_usage(), BudgetUsage::default());
    });
}

async fn send_reply(peer: &mut PendingPeer, cx: &Cx, message: decoder::Message<'_>) {
    let until = clock(cx) + 1_000_000;
    let mut bytes = [0; 512];
    let n = decoder::encode(
        message,
        peer.media.binding(),
        peer.media.limits().protocol(),
        &mut bytes,
        InputDirection::ViewerToHost,
        InputDelivery::Reliable,
    )
    .unwrap();
    loop {
        match peer
            .link
            .c
            .send(cx, Route::Stream(peer.reply), &bytes[..n], until, || true)
        {
            Ok(()) => return,
            Err(quic::Error::Backpressure) => peer.link.drive(cx).await,
            error => panic!("reply: {error:?}"),
        }
    }
}

#[test]
fn pending_expiry_is_idle_driven_terminal_and_does_not_expire_another_decoder() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group_with_timeout(&rt, 2, Duration::from_millis(25))).await;
        let first_deadline = group.peers[0].deadline;
        let second_deadline = group.peers[1].deadline;
        assert!(first_deadline < second_deadline);
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            (first_deadline + 1) * 1000,
        ))
        .await;
        assert_eq!(group.publisher.tick().unwrap(), 1);
        assert_eq!(
            group.peers[0].service(&cx),
            Err(PublishError::Startup(Error::Expired))
        );
        assert!(group.peers[0].control.check().is_err());
        group.peers[1].control.check().unwrap();
        group.owner.check().unwrap();
        // A delayed peer reply cannot restart its expired slot or reset the IDR age.
        send_reply(&mut group.peers[0], &cx, decoder::Message::Configured).await;
        group.peers[0].link.drive(&cx).await;
        assert_eq!(
            group.peers[0].service(&cx),
            Err(PublishError::Startup(Error::Expired))
        );
        assert_eq!(group.peers[1].deadline, second_deadline);
        let mut viewer = Box::pin(configure(&mut group.peers[1], &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[1], &mut viewer, &cx)).await;
        let (mut presenter, mut receiver) =
            Box::pin(acknowledge(&mut group.peers[1], viewer, &cx)).await;
        assert_eq!(group.publisher.capture_next().await.unwrap().frame, 1);
        Box::pin(receive_frame(
            &mut group.peers[1],
            &mut presenter,
            &mut receiver,
            1,
            &cx,
        ))
        .await;
        cleanup(&mut group, &cx).await;
        reap(&mut presenter, &cx).await;
        assert_eq!(group.publisher.physical_usage(), BudgetUsage::default());
    });
}

#[test]
fn wrong_first_decoded_report_refuses_only_the_offending_pending_viewer() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, 2)).await;
        let mut a = Box::pin(configure(&mut group.peers[0], &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[0], &mut a, &cx)).await;
        send_reply(
            &mut group.peers[0],
            &cx,
            decoder::Message::FirstDecoded {
                frame: 99,
                decoder_micros: clock(&cx),
            },
        )
        .await;
        let until = clock(&cx) + 500_000;
        loop {
            assert!(clock(&cx) < until);
            group.peers[0].link.drive(&cx).await;
            if let Err(error) = group.peers[0].service(&cx) {
                assert_eq!(error, PublishError::Startup(Error::WrongState));
                break;
            }
        }
        assert!(group.peers[0].control.check().is_err());
        group.peers[1].control.check().unwrap();
        assert_eq!(group.publisher.tick().unwrap(), 1);
        let mut b = Box::pin(configure(&mut group.peers[1], &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[1], &mut b, &cx)).await;
        let (mut presenter, mut receiver) =
            Box::pin(acknowledge(&mut group.peers[1], b, &cx)).await;
        assert_eq!(group.publisher.capture_next().await.unwrap().frame, 1);
        Box::pin(receive_frame(
            &mut group.peers[1],
            &mut presenter,
            &mut receiver,
            1,
            &cx,
        ))
        .await;
        a.close();
        a.reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        cleanup(&mut group, &cx).await;
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn slow_first_decoded_report_is_fenced_before_the_next_shared_reference() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, 2)).await;
        let mut a = Box::pin(configure(&mut group.peers[0], &cx)).await;
        let mut b = Box::pin(configure(&mut group.peers[1], &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[0], &mut a, &cx)).await;
        Box::pin(decode_without_ack(&mut group.peers[1], &mut b, &cx)).await;
        let (mut presenter, mut receiver) =
            Box::pin(acknowledge(&mut group.peers[0], a, &cx)).await;
        let first = group.publisher.capture_next().await.unwrap();
        assert_eq!((first.frame, first.delivered), (1, 2));
        assert_eq!(group.peers[1].service(&cx).unwrap().accepted, 0);
        Box::pin(receive_frame(
            &mut group.peers[0],
            &mut presenter,
            &mut receiver,
            1,
            &cx,
        ))
        .await;
        let next = group.publisher.capture_next().await.unwrap();
        assert_eq!((next.frame, next.delivered, next.refused), (2, 1, 1));
        assert_eq!(
            group.peers[1].service(&cx),
            Err(PublishError::SlowSubscriber)
        );
        assert!(group.peers[1].control.check().is_err());
        group.owner.check().unwrap();
        Box::pin(receive_frame(
            &mut group.peers[0],
            &mut presenter,
            &mut receiver,
            2,
            &cx,
        ))
        .await;
        b.close();
        b.reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        cleanup(&mut group, &cx).await;
        reap(&mut presenter, &cx).await;
    });
}

#[test]
fn an_equal_numbered_foreign_bootstrap_cannot_enter_the_pending_cohort() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut group = Box::pin(group(&rt, 1)).await;
        let mut link = Link::new(&cx, 100).await;
        let media = link.media(&cx).await;
        let control = gate(&rt, 100);
        let mut foreign = source(&group.owner, true).await;
        let foreign_pool = pool();
        let update = foreign
            .prepare_shared_capture(&group.owner, &foreign_pool)
            .unwrap()
            .capture_if_changed(true)
            .await
            .unwrap();
        assert_eq!(update.frame(), group.initial.as_ref().unwrap().frame());
        let startup = Host::new_shared(
            control.clone(),
            &link.h,
            media
                .host
                .decoder_setup(&link.h, Duration::from_secs(2))
                .unwrap(),
            configuration(),
            update,
        )
        .unwrap();
        let sender = media
            .host
            .sender(&link.h, control.clone(), SendPolicy::default())
            .unwrap();
        let before = group.publisher.physical_usage();
        let sends = link.h.usage().retained_send_records;
        assert!(matches!(
            group
                .publisher
                .admit_pending(startup, sender, media.host, &link.h),
            Err(PublishError::Startup(Error::Media(
                frd::media::Error::InvalidFrame
            )))
        ));
        assert_eq!(group.publisher.tick().unwrap(), 1);
        assert_eq!(group.publisher.physical_usage(), before);
        assert_eq!(foreign_pool.usage(), BudgetUsage::default());
        assert_eq!(link.h.usage().retained_send_records, sends);
        control.check().unwrap();
        group.owner.check().unwrap();
        group.peers[0].control.check().unwrap();
        stop(&mut foreign, &cx).await;
        cleanup(&mut group, &cx).await;
    });
}
