//! Actual TLS/UDP attachments, source IPC, packetization and reassembly. Decoder
//! completions below are explicit fixtures, NOT HEVC/GPU/visibility qualification.
#![cfg(target_os = "linux")]
#[path = "shared_startup/support.rs"]
#[allow(dead_code)]
mod support;
use asupersync::{cx::Cx, runtime::Runtime};
use fr_media::delivery::{BudgetUsage, MediaBudget, ReceivePipeline, ReceivePolicy, SendPolicy};
use fr_transport::quic::{self, Disposition, Messages, Route, StreamRoute};
use fr_wire::{
    decoder,
    input::{InputDelivery, InputDirection},
};
use frd::{
    media::{
        ObservationControl,
        decoder_startup::Host,
        shared_publisher::{Error, JoinQueue, Publisher, Subscriber},
    },
    media_egress::Lane,
    media_quic::NegotiatedMedia,
    worker::Deadline,
};
use std::time::Duration;
use support::*;

struct Peer {
    link: Link,
    host: Option<NegotiatedMedia>,
    media: NegotiatedMedia,
    reply: StreamRoute,
    control: ObservationControl,
    sub: Option<Subscriber>,
    receiver: ReceivePipeline,
    configuration: Option<Vec<u8>>,
    frames: Vec<u64>,
}
impl Peer {
    async fn new(rt: &Runtime, id: u128) -> Self {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx, id).await;
        let media = link.media(&cx).await;
        let config = media
            .viewer
            .receiver_config(&link.c, ReceivePolicy::default())
            .unwrap();
        let receiver =
            ReceivePipeline::new(config, MediaBudget::new(config.limits.protocol()).unwrap())
                .unwrap();
        Self {
            link,
            host: Some(media.host),
            media: media.viewer,
            reply: media.reply,
            control: gate(rt, id),
            sub: None,
            receiver,
            configuration: None,
            frames: vec![],
        }
    }
    fn queue(&mut self, queue: &JoinQueue, timeout: Duration) -> Result<(), Error> {
        let sub = queue.admit(
            self.control.clone(),
            self.host.take().unwrap(),
            &self.link.h,
            SendPolicy::default(),
            timeout,
        )?;
        self.sub = Some(sub);
        Ok(())
    }
    fn service(&mut self, cx: &Cx) -> Result<(), Error> {
        self.sub
            .as_mut()
            .unwrap()
            .service(cx, &mut self.link.h, 1)?;
        Ok(())
    }
    fn ready(&mut self) -> Result<bool, Error> {
        self.sub.as_mut().unwrap().is_ready(&self.link.h)
    }
    async fn network(&mut self, cx: &Cx) {
        self.link.drive(cx).await;
        self.link
            .c
            .receive_ready(
                cx,
                || true,
                |r| matches!(r, Route::Stream(s) if s.messages == Messages::Exact(0x30)),
                |_, bytes| {
                    assert!(self.configuration.is_none(), "configuration sent twice");
                    assert!(matches!(
                        decoder::decode(
                            bytes,
                            self.media.binding(),
                            self.media.limits().protocol(),
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable
                        )
                        .unwrap(),
                        decoder::Message::Configuration(_)
                    ));
                    self.configuration = Some(bytes.to_vec());
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        self.media
            .receive_ready(
                cx,
                &mut self.link.c,
                || true,
                |channel, bytes| {
                    self.receiver.receive(channel, bytes, clock(cx)).unwrap();
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        // Explicit fixture completion through the production dependency checks.
        while let Some(picture) = self.receiver.take_decodable(clock(cx)).unwrap() {
            let decoded = self.receiver.complete_decode(&picture, clock(cx)).unwrap();
            self.frames.push(decoded.descriptor().frame);
        }
    }
    async fn reply(&mut self, cx: &Cx, message: decoder::Message<'_>) {
        if message == decoder::Message::Configured {
            assert!(self.configuration.is_some());
            self.receiver.decoder_configured(clock(cx)).unwrap();
        }
        let until = clock(cx) + 1_000_000;
        let mut bytes = [0; 512];
        let n = decoder::encode(
            message,
            self.media.binding(),
            self.media.limits().protocol(),
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        loop {
            assert!(clock(cx) < until);
            match self
                .link
                .c
                .send(cx, Route::Stream(self.reply), &bytes[..n], until, || true)
            {
                Ok(()) => break,
                Err(quic::Error::Backpressure) => self.network(cx).await,
                other => panic!("reply refused: {other:?}"),
            }
        }
    }
    async fn configured(&mut self, cx: &Cx) {
        let until = clock(cx) + 1_000_000;
        while self.configuration.is_none() {
            assert!(clock(cx) < until);
            self.service(cx).unwrap();
            self.network(cx).await;
        }
        assert!(!self.ready().unwrap());
        // A bootstrap is cached, but not one byte may leave before configured.
        for _ in 0..3 {
            self.service(cx).unwrap();
            self.network(cx).await;
        }
        assert_eq!(self.frames, [] as [u64; 0]);
        self.reply(cx, decoder::Message::Configured).await;
    }
    async fn receive(&mut self, cx: &Cx, frame: u64) {
        let until = clock(cx) + 1_000_000;
        while !self.frames.contains(&frame) {
            assert!(
                clock(cx) < until,
                "frame {frame} missing: {:?}",
                self.frames
            );
            self.service(cx).unwrap();
            self.network(cx).await;
        }
    }
    async fn first_decoded(&mut self, cx: &Cx, frame: u64) {
        assert!(self.frames.contains(&frame));
        assert!(!self.ready().unwrap());
        self.reply(
            cx,
            decoder::Message::FirstDecoded {
                frame,
                decoder_micros: clock(cx),
            },
        )
        .await;
        let until = clock(cx) + 1_000_000;
        while !self.ready().unwrap() {
            assert!(clock(cx) < until);
            self.service(cx).unwrap();
            self.network(cx).await;
        }
        assert!(
            !self.control.view_ready().unwrap(),
            "decode ack cannot grant input"
        );
    }
}
struct Running {
    publisher: Publisher,
    owner: ObservationControl,
    old: Peer,
}
async fn running(rt: &Runtime, delayed: bool) -> Box<Running> {
    let cx = Cx::current().unwrap();
    let owner = gate(rt, 1);
    let mut old = Peer::new(rt, 13).await;
    let mut source = source_variant(&owner, true, true, delayed).await;
    let pool = pool();
    let first = source
        .prepare_shared_capture(&owner, &pool)
        .unwrap()
        .capture_if_changed(true)
        .await
        .unwrap();
    let media = old.host.take().unwrap();
    let mut host = Host::new_shared(
        old.control.clone(),
        &old.link.h,
        media
            .decoder_setup(&old.link.h, Duration::from_secs(2))
            .unwrap(),
        configuration(),
        first.clone(),
    )
    .unwrap();
    let mut sent = false;
    while old.configuration.is_none() {
        if !sent {
            sent = host.transmit(&mut old.link.h).unwrap();
        }
        old.network(&cx).await;
    }
    old.reply(&cx, decoder::Message::Configured).await;
    let bootstrap = loop {
        old.network(&cx).await;
        host.dispatch(&mut old.link.h).unwrap();
        if let Some(update) = host.take_shared_recovery().unwrap() {
            break update;
        }
    };
    let mut sender = media
        .sender(&old.link.h, old.control.clone(), SendPolicy::default())
        .unwrap();
    sender.enqueue_shared_capture(&bootstrap).unwrap();
    while !old.frames.contains(&0) {
        sender
            .transmit(&cx, &mut old.link.h, Lane::Original)
            .unwrap();
        old.network(&cx).await;
    }
    old.reply(
        &cx,
        decoder::Message::FirstDecoded {
            frame: 0,
            decoder_micros: clock(&cx),
        },
    )
    .await;
    while !host.is_complete() {
        old.network(&cx).await;
        host.dispatch(&mut old.link.h).unwrap();
    }
    let mut publisher = Publisher::new(source, owner.clone(), pool, &first).unwrap();
    old.sub = Some(publisher.admit(host, sender, media, &old.link.h).unwrap());
    Box::new(Running {
        publisher,
        owner,
        old,
    })
}
async fn cleanup(run: &mut Running, cx: &Cx) {
    drop(run.old.sub.take());
    run.publisher
        .reap(cx, Deadline::after(cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
    assert_eq!(run.publisher.physical_usage(), BudgetUsage::default());
}

macro_rules! run_late_test {
    ($rt:ident, $cx:ident, $run:ident, $changed:expr, $body:block) => {{
        let $rt = runtime();
        $rt.block_on(async {
            let $cx = Cx::current().unwrap();
            let mut $run = Box::pin(running(&$rt, $changed)).await;
            $body
            cleanup(&mut $run, &$cx).await;
        });
    }};
}

macro_rules! run_shared {
    ($rt:ident, $cx:ident, $body:expr) => {{
        let $rt = runtime();
        $rt.block_on(async {
            let $cx = Cx::current().unwrap();
            $body
        });
    }};
}

#[test]
fn two_late_joins_share_one_fresh_idr_and_continue_without_first_decode_rtt() {
    run_late_test!(rt, cx, run, false, {
        let pid = run.publisher.worker_id();
        let prior = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, prior.frame).await;
        let queue = run.publisher.join_queue();
        let mut a = Peer::new(&rt, 14).await;
        let mut b = Peer::new(&rt, 15).await;
        a.queue(&queue, Duration::from_secs(2)).unwrap();
        b.queue(&queue, Duration::from_secs(2)).unwrap();
        assert_eq!(run.publisher.tick().unwrap(), 1); // Not decoded yet.
        a.service(&cx).unwrap();
        a.network(&cx).await;
        assert!(a.configuration.is_none());
        assert_eq!(a.frames, [] as [u64; 0]);
        let idr = run.publisher.capture_next().await.unwrap();
        assert_eq!((idr.frame, idr.delivered, idr.refused), (2, 3, 0));
        assert!(run.publisher.physical_usage().pictures <= 3);
        run.old.receive(&cx, idr.frame).await;
        a.configured(&cx).await;
        b.configured(&cx).await;
        a.receive(&cx, idr.frame).await;
        b.receive(&cx, idr.frame).await;
        assert!(!a.ready().unwrap());
        assert!(!b.ready().unwrap());
        // No FirstDecoded has been sent. The configured sender already retains
        // the live chain; the source does not wait another round trip per frame.
        let next = run.publisher.capture_next().await.unwrap();
        assert_eq!((next.frame, next.delivered, next.refused), (3, 3, 0));
        run.old.receive(&cx, next.frame).await;
        a.receive(&cx, next.frame).await;
        b.receive(&cx, next.frame).await;
        a.first_decoded(&cx, idr.frame).await;
        b.first_decoded(&cx, idr.frame).await;
        assert_eq!(run.publisher.tick().unwrap(), 3);
        assert_eq!(a.frames, [2, 3]);
        assert_eq!(b.frames, [2, 3]);
        assert_eq!(run.old.frames, [0, 1, 2, 3]);
        assert_eq!(run.publisher.worker_id(), pid);
        drop(a.sub.take());
        let next = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, next.frame).await;
        b.receive(&cx, next.frame).await;
        assert_eq!(next.delivered, 2);
        drop(b.sub.take());
    });
}

#[test]
fn stalled_newcomer_is_refused_before_next_capture_without_stalling_healthy_viewer() {
    run_late_test!(rt, cx, run, false, {
        let mut slow = Peer::new(&rt, 14).await;
        slow.queue(&run.publisher.join_queue(), Duration::from_secs(2))
            .unwrap();
        let idr = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, idr.frame).await;
        slow.service(&cx).unwrap();
        slow.network(&cx).await; // Withhold Configured.
        assert!(slow.configuration.is_some());
        assert_eq!(slow.frames, [] as [u64; 0]);
        // Preserve a SHORT source-bound chain while native configuration is
        // pending, rather than making every 30fps join lose its next reference.
        // Four pictures (bootstrap plus three dependent frames) is the hard cap.
        for _ in 0..3 {
            let next = run.publisher.capture_next().await.unwrap();
            assert_eq!((next.delivered, next.refused), (2, 0));
            run.old.receive(&cx, next.frame).await;
        }
        let next = run.publisher.capture_next().await.unwrap();
        assert_eq!((next.delivered, next.refused), (1, 1));
        assert_eq!(slow.ready(), Err(Error::SlowSubscriber));
        assert!(slow.control.check().is_err());
        run.old.receive(&cx, next.frame).await;
        assert!(run.owner.check().is_ok());
        assert!(run.old.ready().unwrap());
        drop(slow.sub.take());
    });
}

#[test]
fn join_expiry_during_native_capture_does_not_change_healthy_source_deadline() {
    run_late_test!(rt, cx, run, true, {
        let pid = run.publisher.worker_id();
        let mut short = Peer::new(&rt, 14).await;
        short
            .queue(&run.publisher.join_queue(), Duration::from_millis(10))
            .unwrap();
        let next = run.publisher.capture_next().await.unwrap();
        assert_eq!(next.delivered, 1);
        assert_eq!(short.ready(), Err(Error::JoinExpired));
        assert!(short.control.check().is_err());
        run.old.receive(&cx, next.frame).await;
        let again = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, again.frame).await;
        assert_eq!(run.publisher.worker_id(), pid);
        assert!(run.owner.check().is_ok());
        drop(short.sub.take());
    });
}

#[test]
fn waiting_joins_and_queue_handles_cannot_keep_last_viewers_source_alive() {
    run_shared!(rt, cx, {
        let mut run = Box::pin(running(&rt, false)).await;
        let queue = run.publisher.join_queue();
        let mut pending = Peer::new(&rt, 14).await;
        pending.queue(&queue, Duration::from_secs(2)).unwrap();
        drop(run.old.sub.take());
        assert!(run.owner.check().is_err());
        assert!(pending.control.check().is_err());
        assert_eq!(pending.ready(), Err(Error::Closed));
        assert_eq!(run.publisher.physical_usage(), BudgetUsage::default());
        let mut fresh = Peer::new(&rt, 15).await;
        assert!(matches!(
            fresh.queue(&queue, Duration::from_secs(2)),
            Err(Error::Closed)
        ));
        drop(pending.sub.take());
        cleanup(&mut run, &cx).await;
        drop(run);
        let mut later = Peer::new(&rt, 16).await;
        assert_eq!(
            later.queue(&queue, Duration::from_secs(2)),
            Err(Error::Closed)
        );
    });
}

#[test]
fn opaque_connection_and_duplicate_admission_do_not_mutate_an_existing_waiter() {
    run_late_test!(rt, cx, run, false, {
        let queue = run.publisher.join_queue();
        let mut pending = Peer::new(&rt, 14).await;
        pending.queue(&queue, Duration::from_secs(2)).unwrap();
        let usage = run.publisher.physical_usage();
        assert_eq!(
            pending
                .sub
                .as_mut()
                .unwrap()
                .service(&cx, &mut run.old.link.h, 1),
            Err(Error::ForeignConnection)
        );
        let mut duplicate = Peer::new(&rt, 14).await;
        assert_eq!(
            duplicate.queue(&queue, Duration::from_secs(2)),
            Err(Error::WrongSource)
        );
        let mut foreign = Peer::new(&rt, 15).await;
        assert!(matches!(
            queue.admit(
                foreign.control.clone(),
                foreign.host.take().unwrap(),
                &run.old.link.h,
                SendPolicy::default(),
                Duration::from_secs(2)
            ),
            Err(Error::Transport(_))
        ));
        assert!(pending.control.check().is_ok());
        assert!(!pending.ready().unwrap());
        assert_eq!(run.publisher.physical_usage(), usage);
        let idr = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, idr.frame).await;
        pending.configured(&cx).await;
        pending.receive(&cx, idr.frame).await;
        pending.first_decoded(&cx, idr.frame).await;
        drop(pending.sub.take());
    });
}

#[test]
fn all_viewer_pressure_cannot_be_bypassed_by_a_pending_join() {
    run_late_test!(rt, cx, run, false, {
        let mut pending = Peer::new(&rt, 14).await;
        let first = run.publisher.capture_next().await.unwrap(); // Original now has pending output.
        pending
            .queue(&run.publisher.join_queue(), Duration::from_secs(2))
            .unwrap();
        let before = run.publisher.physical_usage();
        assert_eq!(
            run.publisher.capture_next().await,
            Err(Error::Media(frd::media::Error::Backpressure))
        );
        assert_eq!(run.publisher.physical_usage(), before);
        assert!(!pending.ready().unwrap());
        run.old.receive(&cx, first.frame).await;
        let next = run.publisher.capture_next().await.unwrap();
        assert_eq!(next.frame, first.frame + 1);
        assert_eq!(next.delivered, 2);
        run.old.receive(&cx, next.frame).await;
        drop(pending.sub.take());
    });
}

#[test]
fn eight_total_slots_bound_pending_joins_and_drop_returns_only_one_slot() {
    run_late_test!(rt, cx, run, false, {
        let queue = run.publisher.join_queue();
        let mut waiting = Vec::new();
        for id in 14..=20 {
            let mut peer = Peer::new(&rt, id).await;
            peer.queue(&queue, Duration::from_secs(2)).unwrap();
            waiting.push(peer);
        }
        let usage = run.publisher.physical_usage();
        let mut overflow = Peer::new(&rt, 21).await;
        assert_eq!(
            overflow.queue(&queue, Duration::from_secs(2)),
            Err(Error::Full)
        );
        assert!(overflow.control.check().is_ok());
        assert_eq!(run.publisher.physical_usage(), usage);
        assert_eq!(run.publisher.tick().unwrap(), 1);
        drop(waiting[2].sub.take());
        assert!(waiting[2].control.check().is_err());
        let mut replacement = Peer::new(&rt, 22).await;
        replacement.queue(&queue, Duration::from_secs(2)).unwrap();
        assert!(!replacement.ready().unwrap());
        for peer in &mut waiting {
            drop(peer.sub.take());
        }
        drop(replacement.sub.take());
    });
}

#[test]
fn premature_first_decode_and_revoked_source_never_send_a_bootstrap() {
    run_late_test!(rt, cx, run, false, {
        let queue = run.publisher.join_queue();
        let mut bad = Peer::new(&rt, 14).await;
        bad.queue(&queue, Duration::from_secs(2)).unwrap();
        let idr = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, idr.frame).await;
        while bad.configuration.is_none() {
            bad.service(&cx).unwrap();
            bad.network(&cx).await;
        }
        bad.reply(
            &cx,
            decoder::Message::FirstDecoded {
                frame: idr.frame,
                decoder_micros: clock(&cx),
            },
        )
        .await;
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until);
            bad.network(&cx).await;
            if let Err(error) = bad.service(&cx) {
                assert_eq!(
                    error,
                    Error::Startup(frd::media::decoder_startup::Error::WrongState)
                );
                break;
            }
        }
        assert_eq!(bad.frames, [] as [u64; 0]);
        assert!(bad.control.check().is_err());
        assert!(run.old.ready().unwrap());
        assert!(run.owner.check().is_ok());
        drop(bad.sub.take());
        let mut pending = Peer::new(&rt, 15).await;
        pending.queue(&queue, Duration::from_secs(2)).unwrap();
        // Queue remains live without touching the worker; source revocation must
        // block configuration as well as pixels on its original connection.
        run.owner.revoke();
        assert!(pending.service(&cx).is_err());
        pending.network(&cx).await;
        assert!(pending.configuration.is_none());
        assert_eq!(pending.frames, [] as [u64; 0]);
        assert!(pending.control.check().is_err());
        drop(pending.sub.take());
    });
}

#[test]
fn late_join_drives_the_original_supervised_viewer_decoder_handshake() {
    use frd::media::decoder_startup::Viewer;
    run_late_test!(rt, cx, run, false, {
        let mut peer = Peer::new(&rt, 14).await;
        peer.queue(&run.publisher.join_queue(), Duration::from_secs(2))
            .unwrap();
        let idr = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, idr.frame).await;
        while peer.configuration.is_none() {
            peer.service(&cx).unwrap();
            peer.network(&cx).await;
        }
        let mut viewer = Viewer::start(
            cx.clone(),
            &peer.link.c,
            peer.media
                .decoder_setup(&peer.link.c, Duration::from_secs(2))
                .unwrap(),
            peer.configuration.as_ref().unwrap(),
            decoder(),
            peer.media
                .receiver_config(&peer.link.c, ReceivePolicy::default())
                .unwrap(),
        )
        .await
        .unwrap();
        while !viewer.transmit(&mut peer.link.c).unwrap() {
            peer.link.drive(&cx).await;
        }
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until);
            peer.service(&cx).unwrap();
            peer.link.drive(&cx).await;
            peer.media
                .receive_ready(
                    &cx,
                    &mut peer.link.c,
                    || true,
                    |channel, bytes| {
                        viewer.receive_media(channel, bytes).unwrap();
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(receipt) = viewer.present_first().await.unwrap() {
                assert_eq!(receipt.frame.as_raw(), idr.frame);
                break;
            }
        }
        assert!(!peer.ready().unwrap());
        while !viewer.transmit(&mut peer.link.c).unwrap() {
            peer.link.drive(&cx).await;
        }
        while !peer.ready().unwrap() {
            peer.service(&cx).unwrap();
            peer.link.drive(&cx).await;
        }
        let (mut presenter, mut receiver) = viewer.finish().unwrap();
        let frame = run.publisher.capture_next().await.unwrap();
        run.old.receive(&cx, frame.frame).await;
        loop {
            assert!(clock(&cx) < until);
            peer.service(&cx).unwrap();
            peer.link.drive(&cx).await;
            peer.media
                .receive_ready(
                    &cx,
                    &mut peer.link.c,
                    || true,
                    |channel, bytes| {
                        receiver.receive(channel, bytes, clock(&cx)).unwrap();
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
            if let Some(receipt) = presenter.present_next(&cx, &mut receiver).await.unwrap() {
                assert_eq!(receipt.frame.as_raw(), frame.frame);
                break;
            }
        }
        assert!(!peer.control.view_ready().unwrap());
        drop(peer.sub.take());
        receiver.close();
        presenter.abort();
        presenter
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
    });
}

async fn both<A: std::future::Future, B: std::future::Future>(
    a: A,
    b: B,
) -> (A::Output, B::Output) {
    use std::{future::poll_fn, pin::pin, task::Poll};
    let mut a = pin!(a);
    let mut b = pin!(b);
    let mut av = None;
    let mut bv = None;
    poll_fn(|task| {
        if av.is_none()
            && let Poll::Ready(value) = a.as_mut().poll(task)
        {
            av = Some(value);
        }
        if bv.is_none()
            && let Poll::Ready(value) = b.as_mut().poll(task)
        {
            bv = Some(value);
        }
        if av.is_some() && bv.is_some() {
            Poll::Ready((av.take().unwrap(), bv.take().unwrap()))
        } else {
            Poll::Pending
        }
    })
    .await
}

#[test]
fn queue_and_original_connections_run_while_source_service_exclusively_owns_capture() {
    run_late_test!(rt, cx, run, false, {
        let pid = run.publisher.worker_id();
        let queue = run.publisher.join_queue();
        let mut late = Peer::new(&rt, 14).await;
        let mut reports = Vec::new();
        let source = run
            .publisher
            .serve(Duration::from_millis(80), |r| reports.push(r));
        let network = async {
            // Admission uses only the weak queue even while the source owner is
            // exclusively borrowed by its continuing service future.
            late.queue(&queue, Duration::from_secs(2)).unwrap();
            let until = clock(&cx) + 1_500_000;
            while late.configuration.is_none() {
                assert!(clock(&cx) < until);
                run.old.service(&cx).unwrap();
                run.old.network(&cx).await;
                late.service(&cx).unwrap();
                late.network(&cx).await;
                asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
            }
            late.reply(&cx, decoder::Message::Configured).await;
            while late.frames.is_empty() {
                assert!(clock(&cx) < until);
                run.old.service(&cx).unwrap();
                run.old.network(&cx).await;
                late.service(&cx).unwrap();
                late.network(&cx).await;
                asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
            }
            let first = late.frames[0];
            assert_eq!(first, 1);
            late.first_decoded(&cx, first).await;
            for frame in 2..=3 {
                while !late.frames.contains(&frame) || (frame == 2 && !run.old.frames.contains(&2))
                {
                    assert!(clock(&cx) < until);
                    if run.old.sub.is_some() {
                        run.old.service(&cx).unwrap();
                        run.old.network(&cx).await;
                    }
                    late.service(&cx).unwrap();
                    late.network(&cx).await;
                    asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
                }
                if frame == 2 {
                    drop(run.old.sub.take());
                }
            }
            drop(late.sub.take());
        };
        let (result, ()) = Box::pin(both(source, network)).await;
        assert_eq!(result, Err(Error::Closed));
        assert_eq!(
            reports.iter().map(|r| r.delivered).collect::<Vec<_>>(),
            [2, 2, 1]
        );
        assert!(reports.iter().all(|r| r.refused == 0));
        assert_eq!(late.frames, [1, 2, 3]);
        assert_eq!(run.old.frames, [0, 1, 2]);
        assert_eq!(run.publisher.worker_id(), pid);
        assert!(run.owner.check().is_err());
    });
}
