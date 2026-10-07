//! Actual admitted QUIC attachments and child IPC, with the existing synthetic
//! capture fixture. No hardware or lossy-path qualification is asserted here.
use super::*;

#[test]
fn local_failure_waits_for_the_real_request_without_minting_an_encoder_demand() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let Fixture { mut host, mut presenter, viewer, .. } =
            Box::pin(fixture(&c, &h, "late-once", 2_000_000, Role::Observe, true)).await;
        let control = host.stream.control.clone();
        let original_worker = host.worker_id();
        // The fixture returns the first dependent picture after its 250 ms
        // source-anchored deadline. Nothing is falsely labelled peer progress.
        let update = host.stream.source.capture_if_changed(&control, false).await.unwrap();
        let error = host.stream.sender.enqueue_capture(update).unwrap_err();
        assert!(expired_original(&error));
        let session = host.host.session().unwrap();
        let q = &session.opened.transport;
        let routes = session.opened.routes;
        let parent = session.opened.binding;
        let media = host.media.as_ref().unwrap();
        let sender = &mut host.stream.sender;
        let until = media.admit_sender_failure(q, routes, parent, sender).unwrap();
        assert!(!sender.is_closed());
        assert!(!q.is_closed());
        assert!(!control.view_ready().unwrap());
        assert_eq!(host.stream.source.next_recovery_deadline(), None);
        // Repeated local service neither mints a demand nor moves the deadline.
        assert_eq!(media.admit_sender_failure(q, routes, parent, sender).unwrap(), until);
        let mut view = media.binding();
        view.parent = parent;
        let mut bytes = [0; recovery_request::REQUEST_BYTES];
        let len = recovery_request::encode(
            recovery_request::Request {
                reason: recovery_request::Reason::ReferenceExpired,
                last_useful_frame: None,
            },
            view,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            fr_wire::input::InputDirection::ViewerToHost,
            fr_wire::input::InputDelivery::Reliable,
        ).unwrap();
        // A local pseudo-request would make this Coalesced/None. The actual
        // request must be the unique producer of an original-deadline demand.
        let demand = media.admit_recovery_request(q, routes, parent, sender, &bytes[..len])
            .unwrap().expect("first real request must issue the demand");
        assert_eq!(demand.deadline_micros(), until);
        assert!(media.admit_recovery_request(q, routes, parent, sender, &bytes[..len])
            .unwrap().is_none());
        drop(demand);
        assert_eq!(host.worker_id(), original_worker);
        assert_eq!(host.stream.source.next_recovery_deadline(), None);
        drop(viewer);
        host.reap_media(&cleanup, Deadline::after(&cleanup, Duration::from_secs(1)).unwrap())
            .await.unwrap();
        presenter.abort();
        presenter.reap(&cleanup, Deadline::after(&cleanup, Duration::from_secs(1)).unwrap())
            .await.unwrap();
    });
}
