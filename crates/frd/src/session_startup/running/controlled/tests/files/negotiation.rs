//! Only the peer drives its side manually. The host uses public `offer_files` and
//! ordinary drive, not the standalone attachment helper or private maintenance.
use super::*;

fn request(host: &ControlledHost, timeout: Duration) -> ChannelRequest {
    ChannelRequest {
        binding: Binding {
            parent: ControlBinding {
                id: 14,
                ..host.session.binding()
            },
            display: 9,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        },
        ticket: Ticket(414),
        timeout,
    }
}
fn only_application(route: Route, bytes: &[u8]) -> Result<Disposition, ()> {
    assert!(!matches!(route, Route::Stream(r) if r.binding == 14 || r.messages == Messages::Files));
    assert!(
        !bytes
            .get(6..8)
            .is_some_and(|b| (0x0018..=0x001c).contains(&u16::from_be_bytes([b[0], b[1]])))
    );
    block(route, bytes)
}
async fn negotiate(host: &mut ControlledHost, viewer: &mut View, c: &Cx, h: &Cx) -> MediaChannel {
    assert!(host.file_receive_negotiating());
    assert_eq!(host.file_receive_state(), None);
    assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
    let (mut n, mut t) = (3000, 6000);
    let mut peer: Option<MediaChannel> = None;
    let until = now(h).unwrap() + 1_000_000;
    loop {
        assert!(
            now(h).unwrap() < until,
            "normal host turns did not finish attachment"
        );
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
            let (transport, routes) = viewer.session.io().unwrap();
            let mut offer = None;
            transport
                .receive_ready(
                    c,
                    || true,
                    |r| r == Route::Stream(routes.inbound),
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
            if let Some(bytes) = offer {
                peer = Some(
                    viewer
                        .session
                        .accept_media_channel(&bytes, Duration::from_secs(1))
                        .unwrap(),
                );
            }
        }
        if let Some(channel) = &mut peer {
            channel
                .dispatch(viewer.session.io().unwrap().0, c, || true)
                .unwrap();
            channel
                .finish(viewer.session.io().unwrap().0, c, || true)
                .unwrap();
            if channel.is_complete() && !host.file_receive_negotiating() {
                assert!(!host.control().is_stopped());
                return peer.unwrap();
            }
        }
    }
}
#[test]
fn normal_host_turns_negotiate_then_receive_a_verified_object_without_manual_attachment_service() {
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
        let source = directory.0.join("local-source");
        let bytes = vec![37_u8; 120_007];
        fs::write(&source, &bytes).unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                host.offer_files(
                    request(&host, Duration::from_secs(1)),
                    71,
                    directory.configuration(Permission::new(true)),
                )
                .unwrap();
                let channel = negotiate(&mut host, &mut viewer, &c, &h).await;
                assert_eq!(host.file_receive_state(), Some(State::Active));
                assert_eq!(host.file_receive_reason(), None);
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
                sender
                    .begin(
                        viewer.session.io().unwrap().0,
                        File::open(&source).unwrap(),
                        "received",
                    )
                    .unwrap();
                let (mut n, mut t) = (4000, 7000);
                viewer.visible(&c);
                viewer.queue_key(&c, true);
                let until = now(&c).unwrap() + 1_000_000;
                let mut proof = None;
                while proof.is_none() || viewer.receipts == 0 {
                    assert!(now(&c).unwrap() < until);
                    step(&mut host, &mut viewer, &mut sender, &c, &mut n, &mut t).await;
                    proof = proof.or_else(|| sender.take_result());
                }
                assert_eq!(
                    proof.unwrap().outcome,
                    Outcome::HostPublished {
                        bytes: 120_007,
                        publication: Publication::Durable
                    }
                );
                assert_eq!(fs::read(directory.0.join("received")).unwrap(), bytes);
                assert_eq!(effects.lock().unwrap().events, [true]);
                let result = host.file_receive_result();
                assert!(result.is_some());
                assert_eq!(
                    host.offer_files(
                        request(&host, Duration::from_secs(1)),
                        72,
                        directory.configuration(Permission::new(true))
                    ),
                    Err(FileReceiveError::AlreadyAttached)
                );
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
                assert_eq!(host.file_receive_result(), result);
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
fn missing_file_selection_refuses_before_channel_or_disk_admission() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture(&c, &h, None)).await;
        let directory = Directory::new();
        let before = host.io().unwrap().0.next_channel_binding().unwrap();
        assert_eq!(
            host.offer_files(
                request(&host, Duration::from_secs(1)),
                71,
                directory.configuration(Permission::new(true))
            ),
            Err(FileReceiveError::NotNegotiated)
        );
        assert!(!host.file_receive_negotiating());
        assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
        assert_eq!(host.io().unwrap().0.next_channel_binding().unwrap(), before);
        assert!(!host.control().is_stopped());
        host.close();
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}
#[test]
fn local_permission_and_zero_handle_refuse_without_spending_the_one_use_slot() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let before = host.io().unwrap().0.next_channel_binding().unwrap();
        assert_eq!(
            host.offer_files(
                request(&host, Duration::from_secs(1)),
                71,
                directory.configuration(Permission::new(false))
            ),
            Err(FileReceiveError::PermissionRequired)
        );
        assert_eq!(
            host.offer_files(
                request(&host, Duration::from_secs(1)),
                0,
                directory.configuration(Permission::new(true))
            ),
            Err(FileReceiveError::WrongBinding)
        );
        assert_eq!(host.io().unwrap().0.next_channel_binding().unwrap(), before);
        assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
        assert!(!host.control().is_stopped());
        host.offer_files(
            request(&host, Duration::from_secs(1)),
            71,
            directory.configuration(Permission::new(true)),
        )
        .unwrap();
        assert!(host.file_receive_negotiating());
        host.close();
        assert!(!host.file_receive_negotiating());
        assert_eq!(
            host.file_receive_reason(),
            Some(FileReceiveError::Cancelled)
        );
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}
#[test]
fn unacknowledged_offer_expires_without_refresh_or_disk_start() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let start = now(&h).unwrap();
                host.offer_files(
                    request(&host, Duration::from_millis(120)),
                    71,
                    directory.configuration(Permission::new(true)),
                )
                .unwrap();
                let (mut n, mut t) = (3000, 6000);
                let failure = loop {
                    assert!(
                        now(&h).unwrap() < start + 300_000,
                        "file setup slid its deadline"
                    );
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
                    if let Err(error) = result {
                        break error;
                    }
                };
                assert!(matches!(
                    failure,
                    crate::session_startup::Error::Transport(quic::Error::Expired)
                        | crate::session_startup::Error::Renewal(
                            crate::media::renewal::Error::Transport(
                                quic::Error::Expired | quic::Error::Unauthorized
                            )
                        )
                ));
                // Closure cancels the authority context; inspect its retained timer,
                // not an API that requires live authority after the fence.
                assert!(h.timer_driver().unwrap().now().as_nanos() / 1000 >= start + 120_000);
                assert!(host.control().is_stopped());
                assert!(!host.file_receive_negotiating());
                assert_eq!(host.file_receive_state(), None);
                assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
                assert_eq!(host.file_receive_result(), None);
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}
#[test]
fn cancelling_unfinished_file_setup_fences_parent_without_discarding_a_disk_owner() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        host.offer_files(
            request(&host, Duration::from_secs(1)),
            71,
            directory.configuration(Permission::new(true)),
        )
        .unwrap();
        assert_eq!(host.retire_files(), Err(FileReceiveError::Cancelled));
        assert!(host.control().is_stopped());
        assert!(!host.file_receive_negotiating());
        assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
        assert!(host.io().is_err());
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn revoking_permission_during_setup_fences_without_starting_the_disk_worker() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let permission = Permission::new(true);
        host.offer_files(
            request(&host, Duration::from_secs(1)),
            71,
            directory.configuration(permission.clone()),
        )
        .unwrap();
        permission.revoke();
        let (mut n, mut t) = (3000, 6000);
        assert!(
            host.drive(
                Duration::from_millis(1),
                || nonce(&mut n),
                || ticket(&mut t),
                only_application
            )
            .await
            .is_err()
        );
        assert!(host.control().is_stopped());
        assert!(!host.file_receive_negotiating());
        assert_eq!(host.file_receive_state(), None);
        assert_eq!(host.file_receive_result(), None);
        assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}
#[test]
fn dropping_an_unpolled_turn_retires_the_original_pending_file_exchange() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            viewer: _viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        host.offer_files(
            request(&host, Duration::from_secs(1)),
            71,
            directory.configuration(Permission::new(true)),
        )
        .unwrap();
        let (mut n, mut t) = (3000, 6000);
        drop(host.drive(
            Duration::from_millis(1),
            || nonce(&mut n),
            || ticket(&mut t),
            only_application,
        ));
        assert!(host.control().is_stopped());
        assert!(!host.file_receive_negotiating());
        assert_eq!(
            host.file_receive_reason(),
            Some(FileReceiveError::Cancelled)
        );
        assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
        assert!(host.io().is_err());
        assert!(driver.await.handoff_safe());
        assert!(!seat.is_occupied());
    });
}
