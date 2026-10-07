//! Canonical peers over TLS/UDP with the existing synthetic codec subprocess.
//! Inject a sender-side failure before the next service turn, as repair dispatch
//! does. This is not physical desktop, HEVC or impaired-tailnet qualification.
use super::*;

#[test]
fn already_fenced_sender_notifies_a_healthy_peer_and_recovers_on_original_owners() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            viewer,
            media,
            receiver,
            presenter,
            initial,
        } = Box::pin(fixture(&c, &h, "late-once", 2_000_000, Role::Observe, true)).await;
        let control = host.stream.control.clone();
        let original_host = host.worker_id();
        let original_viewer = presenter.worker_id();
        let connection = host.host.session().unwrap().opened.transport.binding();
        let before = host.stream.sender.source_progress().unwrap().unwrap();
        let update = host
            .stream
            .source
            .capture_if_changed(&control, false)
            .await
            .unwrap();
        assert!(expired_original(
            &host.stream.sender.enqueue_capture(update).unwrap_err()
        ));

        // Drive the actual repair handler before the streaming turn snapshots
        // anything. A valid repair is consumed, but cannot repair the lost chain.
        let repair_route = host.stream.sender.stream_repair_route();
        let Route::Stream(repair_stream) = repair_route else {
            panic!("repair must use the admitted reliable lane");
        };
        let mut bytes = [0; 1150];
        let limits = host.media.as_ref().unwrap().limits();
        let len = fr_wire::encode_repair(
            before.descriptor.frame,
            &[fr_wire::RepairRange { start: 0, end: 1 }],
            limits.max_fragments(),
            repair_stream.binding,
            limits,
            &mut bytes,
        )
        .unwrap();
        assert_eq!(
            host.stream.sender.repair(repair_route, &bytes[..len]),
            Ok(crate::media_quic::RepairAdmission::Refused)
        );
        assert!(host.stream.sender.source_progress().unwrap().is_none());
        let failure = host.stream.sender.recovery_progress().unwrap().unwrap();
        assert_eq!(failure.descriptor, before.descriptor);
        assert_eq!(failure.observed_micros, before.observed_micros);
        assert_eq!(failure.pipeline, fr_wire::PipelineState::Failed);
        assert!(!control.view_ready().unwrap());
        assert_eq!(host.stream.sender.cache_usage().bytes, 0);
        assert_eq!(host.stream.source.next_recovery_deadline(), None);

        // The peer is still healthy and received no announcement of the rejected
        // picture. Only the normal host's Failed notification can elicit recovery.
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        let stop = viewer.control();
        let frames = Rc::new(RefCell::new(Vec::new()));
        let seen = frames.clone();
        let start = Instant::now();
        let mut entropy = 8000;
        let (server, client) = Box::pin(support::both(
            host.serve(
                || super::super::super::tests::nonce(&mut entropy),
                || None,
                super::super::super::tests::block,
            ),
            viewer.serve(
                |_, event| {
                    if let Some(event) = event {
                        seen.borrow_mut().push(event.frame.as_raw());
                    }
                    if (start.elapsed() > Duration::from_millis(3300)
                        && seen.borrow().len() >= 4)
                        || start.elapsed() > Duration::from_secs(5)
                    {
                        stop.stop();
                    }
                    Ok(())
                },
                |_| {},
                super::super::super::tests::block,
            ),
        ))
        .await;
        assert!(server.is_err());
        assert!(client.is_err());
        assert_eq!(
            host.statistics().recovered_streams,
            1,
            "host={server:?}, viewer={client:?}"
        );
        assert_eq!(viewer.statistics().recovered_streams, 1);
        assert!(frames.borrow().len() >= 4);
        assert!(!frames.borrow().contains(&1), "expired native output escaped");
        assert!(start.elapsed() >= Duration::from_millis(3300));
        assert_eq!(host.worker_id(), original_host);
        assert_eq!(viewer.worker_id(), original_viewer);
        assert!(
            host.host
                .session()
                .unwrap()
                .opened
                .transport
                .is_bound_to(&connection)
        );
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
