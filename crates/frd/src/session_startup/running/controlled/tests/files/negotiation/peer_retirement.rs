//! Deterministically hold the host between staging Attached and promotion.
use super::*;
use asupersync::net::quic_native::StreamId;
use fr_transport::quic::{ConnectionBinding, ControlRoutes, Priority, QuicRecords, StreamRoute};

fn receive_offer(viewer: &mut View, c: &Cx) -> Option<MediaChannel> {
    let (q, routes) = viewer.session.io().unwrap();
    let mut offer = None;
    q.receive_ready(
        c,
        || true,
        |route| route == Route::Stream(routes.inbound),
        |_, bytes| {
            if bytes.get(6..8) == Some(&(Kind::StreamBinding as u16).to_be_bytes()) {
                assert!(offer.is_none());
                offer = Some(bytes.to_vec());
                Ok(Disposition::Consumed)
            } else {
                Ok(Disposition::Blocked)
            }
        },
    )
    .unwrap();
    offer.map(|bytes| {
        viewer
            .session
            .accept_media_channel(&bytes, Duration::from_secs(1))
            .unwrap()
    })
}

fn pending_host_routes(channel: &MediaChannel) -> ControlRoutes {
    let descriptor = channel.descriptor();
    let outbound = StreamRoute {
        stream: StreamId(descriptor.host_stream),
        binding: descriptor.binding.parent.id,
        messages: Messages::Exact(0x1a),
        priority: Priority::Bulk,
        outbound: true,
        maximum: attachment::GRANT_RECORD_BYTES,
    };
    ControlRoutes {
        outbound,
        inbound: StreamRoute {
            stream: StreamId(descriptor.viewer_stream),
            messages: Messages::Exact(0x19),
            outbound: false,
            ..outbound
        },
    }
}

async fn raw_turn(host: &mut ControlledHost, viewer: &mut View, c: &Cx, h: &Cx) {
    let (result, ()) = Box::pin(support::both(
        host.io()
            .unwrap()
            .0
            .drive(h, Duration::from_millis(1), || true),
        viewer.drive(c),
    ))
    .await;
    result.unwrap();
}

async fn viewer_finished_first(
    host: &mut ControlledHost,
    viewer: &mut View,
    c: &Cx,
    h: &Cx,
) -> (MediaChannel, ControlRoutes) {
    let mut peer: Option<MediaChannel> = None;
    let (mut n, mut t) = (3000, 6000);
    let until = now(h).unwrap() + 1_000_000;
    // Normal host turns authenticate Attach and queue Attached. Stop when that
    // exact stream has an application prefix awaiting native staging; the host
    // cannot have promoted or constructed its receiver at this boundary.
    let routes = loop {
        assert!(now(h).unwrap() < until, "Attached was never queued");
        if let Some(channel) = &mut peer {
            channel
                .transmit(viewer.session.io().unwrap().0, c, || true)
                .unwrap();
        }
        let (result, ()) = Box::pin(support::both(
            host.drive(
                Duration::from_millis(1),
                || nonce(&mut n),
                || ticket(&mut t),
                only_application,
            ),
            viewer.drive(c),
        ))
        .await;
        result.unwrap();
        if peer.is_none() {
            peer = receive_offer(viewer, c);
        }
        if let Some(channel) = &mut peer {
            channel
                .dispatch(viewer.session.io().unwrap().0, c, || true)
                .unwrap();
            let routes = pending_host_routes(channel);
            if !host.io().unwrap().0.send_staged(routes.outbound).unwrap() {
                assert!(host.file_receive_negotiating());
                assert_eq!(host.file_receive_state(), None);
                assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
                break routes;
            }
        }
    };
    let mut peer = peer.unwrap();
    loop {
        assert!(now(h).unwrap() < until, "viewer never received Attached");
        // Raw host I/O flushes Attached and receives UDP without another turn
        // of the pending attachment owner. The viewer alone may now complete.
        raw_turn(host, viewer, c, h).await;
        peer.dispatch(viewer.session.io().unwrap().0, c, || true)
            .unwrap();
        if peer
            .finish(viewer.session.io().unwrap().0, c, || true)
            .unwrap()
            .is_some()
        {
            assert!(host.file_receive_negotiating());
            assert!(host.io().unwrap().0.send_staged(routes.outbound).unwrap());
            assert_eq!(host.file_receive_state(), None);
            return (peer, routes);
        }
    }
}

async fn admit_source_offer(
    sender: &mut Sender<'_>,
    host: &mut ControlledHost,
    viewer: &mut View,
    c: &Cx,
    h: &Cx,
    directory: &Directory,
) {
    let source = directory.0.join("source");
    fs::write(&source, [37_u8; 257]).unwrap();
    sender
        .begin(
            viewer.session.io().unwrap().0,
            File::open(&source).unwrap(),
            "discarded",
        )
        .unwrap();
    let until = now(c).unwrap() + 1_000_000;
    loop {
        assert!(now(c).unwrap() < until, "source offer was never admitted");
        if sender
            .service(viewer.session.io().unwrap().0, || true)
            .unwrap()
            == Stage::AwaitingAcceptance
        {
            break;
        }
        raw_turn(host, viewer, c, h).await;
    }
    assert!(bulk_pending(viewer.session.io().unwrap().0));
    // Wait for genuine reliable-send retirement. The native host acknowledges
    // a valid ATP manifest, but its file owner never consumes or parses it.
    while bulk_pending(viewer.session.io().unwrap().0) {
        assert!(now(c).unwrap() < until, "native file prefix did not drain");
        raw_turn(host, viewer, c, h).await;
    }
    assert!(host.file_receive_negotiating());
    assert_eq!(host.file_receive_state(), None);
    assert_eq!(sender.stage(), Stage::AwaitingAcceptance);
}

fn bulk_pending(q: &QuicRecords) -> bool {
    let usage = q.usage();
    usage.retained_send_records != usage.critical_send_records
        || usage.retained_send_upper_bound != usage.critical_send_bytes
}

fn assert_no_receiver(
    host: &mut ControlledHost,
    directory: &Directory,
    connection: &ConnectionBinding,
) {
    assert!(!host.file_receive_negotiating());
    assert_eq!(host.file_receive_state(), None);
    assert_eq!(host.file_receive_progress(), None);
    assert_eq!(host.file_receive_result(), None);
    assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
    assert_eq!(
        host.file_receive_reason(),
        Some(FileReceiveError::Cancelled)
    );
    assert_eq!(
        host.offer_files(
            request(host, Duration::from_secs(1)),
            72,
            directory.configuration(Permission::new(true)),
        ),
        Err(FileReceiveError::AlreadyAttached)
    );
    assert!(host.io().unwrap().0.is_bound_to(connection));
    assert!(!directory.0.join("discarded").exists());
    assert!(fs::read_dir(&directory.0).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".fr-part-")
    }));
}

async fn continue_input(
    host: &mut ControlledHost,
    viewer: &mut View,
    c: &Cx,
    n: &mut u128,
    t: &mut u128,
) {
    let until = now(c).unwrap() + 1_000_000;
    for (pressed, receipts) in [(true, 1), (false, 2)] {
        viewer.visible(c);
        viewer.queue_key(c, pressed);
        while viewer.receipts < receipts {
            assert!(now(c).unwrap() < until, "input stopped with the file lane");
            turn(host, viewer, c, n, t).await;
        }
    }
}

fn cancelled_before_receiver_starts(payload: bool) {
    run(move |c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            effects,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let connection = host.io().unwrap().0.binding();
        let ((), shutdown) = Box::pin(support::both(
            async {
                host.offer_files(
                    request(&host, Duration::from_secs(1)),
                    71,
                    directory.configuration(Permission::new(true)),
                )
                .unwrap();
                let (channel, pending) =
                    viewer_finished_first(&mut host, &mut viewer, &c, &h).await;
                let mut lane = FilesChannel::new(
                    viewer.session.io().unwrap().0,
                    channel,
                    credentials().lease,
                    71,
                )
                .unwrap();
                let mut sender = Sender::new(
                    c.clone(),
                    viewer.session.io().unwrap().0,
                    &mut lane,
                    fr_files::sender::Policy::default(),
                )
                .unwrap();
                if payload {
                    admit_source_offer(&mut sender, &mut host, &mut viewer, &c, &h, &directory)
                        .await;
                }
                sender.cancel(viewer.session.io().unwrap().0).unwrap();
                if payload {
                    assert_eq!(
                        sender.result().unwrap().outcome,
                        Outcome::InterruptedBeforePublication(fr_files::sender::Error::Cancelled)
                    );
                } else {
                    assert_eq!(sender.result(), None);
                }
                let until = now(&h).unwrap() + 1_000_000;
                while !host.io().unwrap().0.receive_ended(pending.inbound).unwrap() {
                    assert!(now(&h).unwrap() < until, "host missed native reset");
                    raw_turn(&mut host, &mut viewer, &c, &h).await;
                }
                assert!(host.file_receive_negotiating());
                assert_eq!(host.file_receive_state(), None);
                let (mut n, mut t) = (4000, 7000);
                let (result, ()) = Box::pin(support::both(
                    host.drive(
                        Duration::from_millis(1),
                        || nonce(&mut n),
                        || ticket(&mut t),
                        only_application,
                    ),
                    viewer.drive(&c),
                ))
                .await;
                result.unwrap();
                assert_no_receiver(&mut host, &directory, &connection);
                continue_input(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                while sender.try_finish_cleanup().is_none() {
                    assert!(now(&h).unwrap() < until, "source did not finish cleanup");
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert_eq!(sender.try_finish_cleanup(), Some(Ok(())));
                assert!(!host.control().is_stopped());
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(effects.lock().unwrap().events, [true, false]);
    });
}

#[test]
fn acknowledged_file_offer_before_host_promotion_completes_on_ordinary_control_turns() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            effects,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let connection = host.io().unwrap().0.binding();
        let ((), shutdown) = Box::pin(support::both(
            async {
                host.offer_files(
                    request(&host, Duration::from_secs(1)),
                    71,
                    directory.configuration(Permission::new(true)),
                )
                .unwrap();
                let (channel, _) = viewer_finished_first(&mut host, &mut viewer, &c, &h).await;
                let mut lane = FilesChannel::new(
                    viewer.session.io().unwrap().0,
                    channel,
                    credentials().lease,
                    71,
                )
                .unwrap();
                let mut sender = Sender::new(
                    c.clone(),
                    viewer.session.io().unwrap().0,
                    &mut lane,
                    fr_files::sender::Policy::default(),
                )
                .unwrap();
                admit_source_offer(&mut sender, &mut host, &mut viewer, &c, &h, &directory).await;
                assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
                assert_eq!(sender.result(), None);
                assert!(!directory.0.join("discarded").exists());
                let (mut n, mut t) = (4000, 7000);
                let proof = complete_file_with_input(
                    &mut host,
                    &mut viewer,
                    &mut sender,
                    &c,
                    &mut n,
                    &mut t,
                )
                .await;
                assert_eq!(
                    proof.outcome,
                    Outcome::HostPublished {
                        bytes: 257,
                        publication: Publication::Durable,
                    }
                );
                assert_eq!(
                    fs::read(directory.0.join("discarded")).unwrap(),
                    [37_u8; 257]
                );
                assert_eq!(host.file_receive_state(), Some(State::Active));
                assert!(!host.file_receive_negotiating());
                let published = host.file_receive_result().unwrap();
                sender.cancel(viewer.session.io().unwrap().0).unwrap();
                host.retire_files().unwrap();
                let cleanup = host
                    .reap_files(
                        &c,
                        crate::worker::Deadline::after(&c, Duration::from_secs(1)).unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(matches!(cleanup, FileReceiveCleanup::Finished(_)));
                assert_eq!(host.file_receive_result(), Some(published));
                assert!(host.io().unwrap().0.is_bound_to(&connection));
                assert!(!host.control().is_stopped());
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(effects.lock().unwrap().events, [true, false]);
    });
}

async fn complete_file_with_input(
    host: &mut ControlledHost,
    viewer: &mut View,
    sender: &mut Sender<'_>,
    c: &Cx,
    n: &mut u128,
    t: &mut u128,
) -> fr_files::sender::Receipt {
    let until = now(c).unwrap() + 2_000_000;
    viewer.visible(c);
    viewer.queue_key(c, true);
    let mut proof = None;
    while proof.is_none() || viewer.receipts < 1 {
        assert!(now(c).unwrap() < until, "file transfer or input stopped");
        step(host, viewer, sender, c, n, t).await;
        proof = proof.or_else(|| sender.take_result());
    }
    viewer.visible(c);
    viewer.queue_key(c, false);
    while viewer.receipts < 2 {
        assert!(now(c).unwrap() < until, "input release stopped");
        step(host, viewer, sender, c, n, t).await;
    }
    proof.unwrap()
}

#[test]
fn peer_cancel_after_attached_keeps_control_and_never_starts_file_receiver() {
    cancelled_before_receiver_starts(false);
}

#[test]
fn peer_cancel_after_first_offer_discards_files_without_starting_receiver() {
    cancelled_before_receiver_starts(true);
}
