//! Real canonical loops, TLS/UDP and child IPC; codec/presentation are fixtures.
//! No packet is lost in this fixture: only the host can discover the late frame.
#[allow(clippy::wildcard_imports)]
use super::*;
use crate::session_startup::running::streaming::tests::{block, nonce};

#[test]
fn late_unannounced_capture_recovers_on_the_same_workers_and_connection() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            viewer,
            media,
            receiver,
            presenter,
            initial,
        } = Box::pin(fixture(&c, &h, "late", 2_000_000, Role::Observe, true)).await;
        let host_worker = host.worker_id();
        let viewer_worker = presenter.worker_id();
        let connection = host.host.session().unwrap().opened.transport.binding();
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        let stop = viewer.control();
        let frames = Rc::new(RefCell::new(Vec::new()));
        let seen = frames.clone();
        let started = Instant::now();
        let mut entropy = 9000;
        let (server, client) = Box::pin(support::both(
            host.serve(|| nonce(&mut entropy), || None, block),
            viewer.serve(
                |_, event| {
                    if let Some(event) = event {
                        seen.borrow_mut().push(event.frame.as_raw());
                    }
                    if seen.borrow().contains(&4) || started.elapsed() > Duration::from_secs(4) {
                        stop.stop();
                    }
                    Ok(())
                },
                |_| {},
                block,
            ),
        ))
        .await;
        assert!(server.is_err());
        assert!(client.is_err());
        assert_eq!(host.statistics().expired_capture_updates, 1);
        assert_eq!(host.statistics().recovered_streams, 1, "{server:?}");
        assert_eq!(viewer.statistics().recovered_streams, 1, "{client:?}");
        assert_eq!(host.worker_id(), host_worker);
        assert_eq!(viewer.worker_id(), viewer_worker);
        assert!(host_worker.is_some() && viewer_worker.is_some());
        assert!(
            host.host
                .session()
                .unwrap()
                .opened
                .transport
                .is_bound_to(&connection)
        );
        assert!(
            !frames.borrow().contains(&1),
            "expired output must never be presented"
        );
        for frame in [2, 3, 4] {
            assert!(
                frames.borrow().contains(&frame),
                "recovery IDR and subsequent pictures missing: {:?}; {server:?}; {client:?}",
                frames.borrow()
            );
        }
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}

#[test]
fn repeated_late_captures_exhaust_the_original_subscription_not_a_fresh_allowance() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            viewer,
            media,
            receiver,
            presenter,
            initial,
        } = Box::pin(fixture(
            &c,
            &h,
            "late-repeat",
            2_000_000,
            Role::Observe,
            true,
        ))
        .await;
        let host_worker = host.worker_id();
        let viewer_worker = presenter.worker_id();
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        let stop = viewer.control();
        let frames = Rc::new(RefCell::new(Vec::new()));
        let seen = frames.clone();
        let started = Instant::now();
        let mut entropy = 12_000;
        let (server, client) = Box::pin(support::both(
            host.serve(|| nonce(&mut entropy), || None, block),
            viewer.serve(
                |_, event| {
                    if let Some(event) = event {
                        seen.borrow_mut().push(event.frame.as_raw());
                    }
                    // A broken implementation that refills the allowance must
                    // fail this test, not run an infinite recovery loop.
                    if started.elapsed() > Duration::from_secs(6) {
                        stop.stop();
                    }
                    Ok(())
                },
                |_| {},
                block,
            ),
        ))
        .await;
        assert!(
            matches!(
                &server,
                Err(Error::MediaTransport(crate::media_quic::Error::Media(
                    crate::media::Error::Send(
                        fr_media::delivery::SendError::RecoveryLimitExceeded
                    )
                )))
            ),
            "the second failed generation must be refused: {server:?}"
        );
        assert!(client.is_err());
        assert_eq!(host.statistics().expired_capture_updates, 2);
        assert_eq!(host.statistics().recovered_streams, 1);
        assert_eq!(viewer.statistics().recovered_streams, 1);
        assert_eq!(host.worker_id(), host_worker);
        assert_eq!(viewer.worker_id(), viewer_worker);
        assert!(frames.borrow().contains(&2), "{:?}", frames.borrow());
        assert!(frames.borrow().iter().all(|frame| [0, 2].contains(frame)));
        assert!(host.stream.control.check().is_err());
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}

#[test]
fn late_capture_without_recovery_negotiation_remains_terminal() {
    refuses("late", false);
}

#[test]
fn a_late_capture_cannot_recover_with_a_dependent_picture_instead_of_an_idr() {
    refuses("late-ignore-force", true);
}

fn refuses(mode: &'static str, enabled: bool) {
    run(move |c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            viewer,
            media,
            receiver,
            presenter,
            initial,
        } = Box::pin(fixture(&c, &h, mode, 1_000_000, Role::Observe, enabled)).await;
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        let frames = Rc::new(RefCell::new(Vec::new()));
        let seen = frames.clone();
        let mut entropy = 10_000;
        let (server, client) = Box::pin(support::both(
            host.serve(|| nonce(&mut entropy), || None, block),
            viewer.serve(
                |_, event| {
                    if let Some(event) = event {
                        seen.borrow_mut().push(event.frame.as_raw());
                    }
                    Ok(())
                },
                |_| {},
                block,
            ),
        ))
        .await;
        assert!(server.is_err());
        assert!(client.is_err());
        if !enabled {
            assert!(matches!(
                server,
                Err(Error::MediaTransport(error)) if expired_original(&error)
            ));
        }
        assert_eq!(host.statistics().expired_capture_updates, 1);
        assert_eq!(host.statistics().recovered_streams, 0);
        assert_eq!(viewer.statistics().recovered_streams, 0);
        assert!(frames.borrow().iter().all(|&frame| frame == 0));
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        viewer
            .reap_media(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}

#[test]
fn waiting_for_the_peers_request_is_inside_the_original_failure_budget() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            mut viewer,
            mut presenter,
            ..
        } = Box::pin(fixture(&c, &h, "late", 400_000, Role::Observe, true)).await;
        let started = Instant::now();
        let mut entropy = 11_000;
        // Keep control/renewal alive but deliberately consume the failure notice
        // without creating a receiver recovery request. The host must neither
        // replace this peer's decoder unilaterally nor wait indefinitely.
        let peer = async {
            loop {
                viewer
                    .drive(Duration::from_millis(1), |_, _| Ok(Disposition::Consumed))
                    .await?;
                if started.elapsed() > Duration::from_secs(3) {
                    return Err::<(), Error>(Error::Expired);
                }
            }
        };
        let (server, client) = Box::pin(support::both(
            host.serve(|| nonce(&mut entropy), || None, block),
            peer,
        ))
        .await;
        assert!(server.is_err());
        assert!(client.is_err());
        assert_eq!(host.statistics().expired_capture_updates, 1);
        assert_eq!(host.statistics().recovered_streams, 0);
        assert!(started.elapsed() >= Duration::from_millis(650));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(host.stream.control.check().is_err());
        host.reap_media(
            &cleanup,
            Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
        presenter.abort();
        presenter
            .reap(
                &cleanup,
                Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
    });
}
