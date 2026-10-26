//! A stale report owner refuses the real managed grant handoff. TLS/UDP,
//! managed approval and supervised cleanup run normally; the sink is a fixture.
use super::*;

#[test]
#[allow(clippy::too_many_lines)]
fn a_consumed_pregrant_report_fences_both_consumed_owners_when_the_lease_arrives() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (closed, configuration) = session_startup::closure_reporting::Reporting::new(&cleanup);
        let (mut host, mut viewer) = Box::pin(prepare_with_reporting(
            &c,
            &h,
            "unchanged",
            keys(),
            Some(configuration),
        ))
        .await;
        // Listener abandonment is not a replacement reporting capability.
        drop(closed);
        let target = host.control_target(keys()).unwrap();
        let request = viewer.control_request(1, keys()).unwrap();
        let observation = host.control();
        let viewer_stop = viewer.control();
        let seat = Seat::default();
        let effects = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::new(AtomicU64::new(0));
        let approved = Cell::new(false);
        let mut nonce = 75_000;
        let (report, _) = Box::pin(support::both(
            async {
                let report = host
                    .serve_managed_control_with_cleanup(
                        &cleanup,
                        seat.clone(),
                        |state| {
                            let ManagedHostControlState::Pending(mut pending) = state else {
                                panic!("an invalid report handoff became active control");
                            };
                            if pending.request().is_some()
                                && pending.native_status().is_none()
                                && pending.view_ready()?
                            {
                                assert!(!approved.replace(true));
                                let effects = effects.clone();
                                let release_at = released.clone();
                                let cx = h.clone();
                                pending.approve(
                                    target,
                                    || {
                                        Some((
                                            InputLeaseId::from_raw(619),
                                            InputTicketId::from_raw(623),
                                        ))
                                    },
                                    move || {
                                        Ok(Sink {
                                            effects,
                                            release_at,
                                            cx,
                                        })
                                    },
                                    |_| true,
                                )?;
                            }
                            Ok(Some(target))
                        },
                        || {
                            nonce += 1;
                            Ok(nonce)
                        },
                        || panic!("failed promotion cannot issue tickets"),
                    )
                    .await;
                viewer_stop.stop();
                report
            },
            viewer.serve_requesting_control(
                1,
                keys(),
                InputPolicy::default(),
                |state, _| {
                    if let ViewerControlState::Requesting(pending) = state {
                        pending
                            .confirm_mapping(request.parent, target.view)
                            .unwrap();
                        if let Some(presentation) = pending.presentation() {
                            let _ = pending.visible(presentation.frame.as_raw());
                        }
                    }
                    Ok(())
                },
                |_| panic!("no input was submitted"),
            ),
        ))
        .await;
        assert!(approved.get(), "the granted owner's handoff was exercised");
        assert!(report.session.is_err());
        assert_eq!(
            report.revocation, None,
            "the refused consumer stayed unarmed"
        );
        assert!(report.input.unwrap().handoff_safe());
        assert!(observation.check().is_err());
        assert!(!seat.is_occupied());
        assert!(effects.lock().unwrap().is_empty());
        assert!(!cleanup.is_cancel_requested());
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
