//! The canonical viewer must fence and retain native cleanup before retry policy.
use super::*;
use crate::{
    media_quic::recovery::Error as RecoveryError,
    native_connection::reconnect::{Failure, retryable},
    session_startup::{ObserverError, StreamingViewerError},
};

#[test]
fn canonical_viewer_fences_original_native_owners_before_exposing_namespace_retry() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture {
            mut host,
            mut viewer,
            media,
            receiver,
            presenter,
            initial,
        } = Box::pin(fixture(&c, &h, "healthy", 2_000_000, Role::Observe, true)).await;
        // Reserve a second real three-role set on both original endpoints. This
        // tests the canonical exit/cleanup, not a second end-to-end recovery.
        // recovery_replacement/exhaustion.rs separately consumes the namespace
        // through an actual completed replacement and checks the same cause.
        for (role, id) in [
            (MediaRole::Configuration, 21),
            (MediaRole::Recovery, 22),
            (MediaRole::Video, 23),
        ] {
            let _ = attach(host.host.session().unwrap(), &mut viewer, &c, &h, role, id).await;
        }
        assert_eq!(viewer.io().unwrap().0.remaining_channel_pairs(), 1);
        let original_host = host.worker_id();
        let original_viewer = presenter.worker_id();
        assert!(original_host.is_some() && original_viewer.is_some());
        let authority = host.stream.control.clone();
        let mut viewer =
            StreamingViewer::from_test_parts(viewer, media, presenter, receiver, initial);
        let control = viewer.control();
        // The fixture deliberately withheld an actual sender reference. Let its
        // original deadline elapse; do not fabricate a pipeline error or mutate
        // the live transport's counters to simulate exhaustion.
        asupersync::time::sleep(c.now(), Duration::from_millis(150)).await;
        let mut entropy = 30_000;
        let mut completed = 0;
        let (server, client) = Box::pin(support::both(
            host.serve(
                || super::super::super::tests::nonce(&mut entropy),
                || None,
                super::super::super::tests::block,
            ),
            viewer.serve(
                |input, event| {
                    assert!(input.is_none());
                    completed += usize::from(event.is_some());
                    Ok(())
                },
                |_| {},
                super::super::super::tests::block,
            ),
        ))
        .await;
        let expected = StreamingViewerError::Recovery(RecoveryError::NamespaceExhausted(
            recovery_request::Reason::ReferenceExpired,
        ));
        assert_eq!(client, Err(expected));
        assert!(server.is_err());
        assert!(retryable(Failure::Observation(ObserverError::Streaming(
            expected
        ))));
        assert!(control.is_stopped());
        assert!(authority.check().is_err());
        assert_eq!(completed, 0);
        assert_eq!(viewer.statistics().recovered_streams, 0);
        assert_eq!(
            viewer.budget_usage(),
            fr_media::delivery::BudgetUsage::default()
        );
        // Reconnect cannot discard the original native obligations. They stay
        // collectable until the supervisor's independent cleanup is confirmed.
        assert_eq!(host.worker_id(), original_host);
        assert_eq!(viewer.worker_id(), original_viewer);
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
