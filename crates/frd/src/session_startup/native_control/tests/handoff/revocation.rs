//! Public managed publication, real UDP/TLS and supervised source IPC. Native
//! effects and visibility are explicit fixtures, not X11 or live-tailnet proof.
use super::*;
use crate::{
    input_watchdog::StopReason,
    session_startup::{
        ControlledViewerError, ManagedHostControlState, ObserverError, StreamingViewerError,
    },
};
use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason};

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Native,
    Observation,
}

#[allow(clippy::too_many_lines)]
async fn exercise(c: Cx, h: Cx, end: End) {
    let cleanup = Cx::current().unwrap();
    // Use the caller's separate context, never the revoked session context.
    let report_cx = cleanup.clone();
    let (mut host, mut viewer) = Box::pin(prepare(&c, &h, "unchanged", keys())).await;
    let target = host.control_target(keys()).unwrap();
    let request = viewer.control_request(1, keys()).unwrap();
    let stop = host.control();
    let view_stop = viewer.control();
    let seat = Seat::default();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let released = Arc::new(AtomicU64::new(0));
    let receipts = Cell::new(0);
    let fenced = Cell::new(false);
    let mut since = None;
    let mut action_sent = false;
    let mut nonce = 55_000;
    let mut ticket = 65_000;
    let started = now(&h).unwrap();
    let (report, vr) = Box::pin(support::both(
        async {
            let report = host
                .serve_managed_control_with_cleanup(
                    &report_cx,
                    seat.clone(),
                    |state| {
                        match state {
                            ManagedHostControlState::Pending(mut pending) => {
                                if pending.request().is_none()
                                    || pending.native_status().is_some()
                                    || !pending.view_ready()?
                                {
                                    return Ok(Some(target));
                                }
                                let effects = effects.clone();
                                let release_at = released.clone();
                                let cx = h.clone();
                                pending.approve(
                                    target,
                                    || {
                                        Some((
                                            InputLeaseId::from_raw(519),
                                            InputTicketId::from_raw(523),
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
                            ManagedHostControlState::Active { control, .. } => {
                                let at = *since.get_or_insert_with(|| now(&h).unwrap());
                                if receipts.get() == 1
                                    && now(&h).unwrap() >= at + 85_000
                                    && !fenced.replace(true)
                                {
                                    match end {
                                        End::Native => control.stop(StopReason::LocalRevoke),
                                        End::Observation => stop.revoke(),
                                    }
                                }
                            }
                        }
                        Ok(Some(target))
                    },
                    || {
                        nonce += 1;
                        Ok(nonce)
                    },
                    || {
                        ticket += 1;
                        Some(InputTicketId::from_raw(ticket))
                    },
                )
                .await;
            // Only a missing report ends the peer explicitly. Successful report
            // delivery must terminate the viewer through its actual wire record.
            if !matches!(
                report.revocation,
                Some(Ok(()) | Err(fr_transport::quic::Error::Expired))
            ) {
                view_stop.stop();
            }
            report
        },
        async {
            viewer
                .serve_requesting_control(
                    1,
                    keys(),
                    InputPolicy::default(),
                    |state, frame| {
                        match state {
                            ViewerControlState::Requesting(pending) => {
                                pending
                                    .confirm_mapping(request.parent, target.view)
                                    .unwrap();
                                if let Some(p) = pending.presentation() {
                                    let _ = pending.visible(p.frame.as_raw());
                                }
                            }
                            ViewerControlState::Controlled(input) => {
                                if let Some(p) = frame {
                                    let _ = input.visible(p.frame.as_raw());
                                }
                                if !action_sent {
                                    let _ = input.action(key(true)).unwrap();
                                    action_sent = true;
                                }
                            }
                            ViewerControlState::Observing => panic!("lost control intent"),
                        }
                        assert!(now(&c).unwrap() < started + 2_000_000, "unbounded shutdown");
                        Ok(())
                    },
                    |_| receipts.set(receipts.get() + 1),
                )
                .await
        },
    ))
    .await;
    assert!(fenced.get() && action_sent);
    assert!(report.session.is_err());
    assert_eq!(
        receipts.get(),
        1,
        "cleanup cannot fabricate an action receipt"
    );
    assert!(report.input.unwrap().handoff_safe(), "{report:?}");
    assert!(!seat.is_occupied());
    let ops = effects.lock().unwrap().clone();
    assert_eq!(ops.len(), 2, "press then cleanup release only");
    assert!(matches!(
        ops[1],
        Operation::Key {
            transition: KeyTransition::Release,
            ..
        }
    ));
    assert!(released.load(Ordering::Acquire) != 0);
    let Err(ObserverError::Streaming(StreamingViewerError::Control(
        ControlledViewerError::LeaseRevoked(revoked),
    ))) = vr
    else {
        panic!("missing actual revocation: {vr:?}; {report:?}");
    };
    assert_eq!(revoked.lease, InputLeaseId::from_raw(519));
    if end == End::Native {
        assert_eq!(revoked.reason, Reason::LocalRevoke);
    }
    assert_eq!(revoked.cleanup, CleanupStage::Fenced);
    assert_eq!(revoked.effects, EffectStage::Unknown);
    assert!(h.is_cancel_requested());
    assert!(
        !cleanup.is_cancel_requested(),
        "original session cancellation was not cleared"
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
}

#[test]
fn managed_native_revoke_reports_without_waiting_for_capture_or_releasing_authority() {
    run(|c, h| exercise(c, h, End::Native));
}
#[test]
fn managed_observation_revoke_reports_with_a_separate_uncancelled_cleanup_context() {
    run(|c, h| exercise(c, h, End::Observation));
}

#[test]
fn ending_before_a_grant_never_manufactures_a_lease_revocation() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer) = Box::pin(prepare(&c, &h, "unchanged", keys())).await;
        let stop = host.control();
        let seat = Seat::default();
        let service = host.serve_managed_control_with_cleanup(
            &cleanup,
            seat.clone(),
            |_| panic!("cancelled session cannot grant consent"),
            || panic!("cancelled session cannot mint a nonce"),
            || panic!("cancelled session cannot issue tickets"),
        );
        stop.revoke();
        let report = service.await;
        assert!(report.session.is_err());
        assert_eq!(report.revocation, None, "no granted lease, no host report");
        assert!(report.input.is_none());
        assert!(!seat.is_occupied());
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
fn unpolled_reporting_service_is_send_and_abandonment_still_fences() {
    fn send<T: Send>(_: &T) {}
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer) = Box::pin(prepare(&c, &h, "unchanged", keys())).await;
        let stop = host.control();
        let service = host.serve_managed_control_with_cleanup(
            &cleanup,
            Seat::default(),
            |_| panic!("unpolled consent"),
            || panic!("unpolled nonce"),
            || panic!("unpolled ticket"),
        );
        send(&service);
        drop(service);
        assert!(stop.check().is_err());
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
