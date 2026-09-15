//! Public managed service with actual UDP/TLS and the supervised source fixture.
//! No separate input-driver polling task is installed by these tests.
use super::*;
use crate::session_startup::{ManagedControlReport, ManagedHostControlState};
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Renewing,
    CaptureStall,
    NoConsent,
    NoVisibility,
    FactoryFails,
    FactoryStall,
    CallbackFails,
    Abandoned,
}
struct Unblock(Arc<AtomicBool>);
impl Drop for Unblock {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[allow(clippy::too_many_lines)]
async fn managed(c: Cx, h: Cx, mode: Mode) {
    let cleanup = Cx::current().unwrap();
    let (mut host, mut viewer) = Box::pin(prepare(
        &c,
        &h,
        if mode == Mode::CaptureStall {
            "stall"
        } else {
            "unchanged"
        },
        keys(),
    ))
    .await;
    let target = host.control_target(keys()).unwrap();
    let request = viewer.control_request(1, keys()).unwrap();
    let host_pid = host.worker_id();
    let viewer_pid = viewer.worker_id();
    let stop = host.control();
    let view_stop = viewer.control();
    let seat = Seat::default();
    let effects = Arc::new(Mutex::new(Vec::new()));
    let released = Arc::new(AtomicU64::new(0));
    let factories = Arc::new(AtomicUsize::new(0));
    let gate = Unblock(Arc::new(AtomicBool::new(mode != Mode::FactoryStall)));
    let receipts = Cell::new(0);
    let mut granted = false;
    let mut action_sent = false;
    let mut shown = None;
    let mut since = None;
    let mut approval_count = 0;
    let mut nonce = 25_000;
    let mut ticket = 35_000;
    let start = now(&h).unwrap();
    let (report, vr) = Box::pin(support::both(
        async {
            let mut service = Box::pin(host.serve_managed_control(
                seat.clone(),
                |state| {
                    if let ManagedHostControlState::Pending(mut pending) = state
                        && mode != Mode::NoConsent
                        && pending.request().is_some()
                        && pending.native_status().is_none()
                        && pending.view_ready()?
                    {
                        let effects = effects.clone();
                        let release_at = released.clone();
                        let calls = factories.clone();
                        let unblocked = gate.0.clone();
                        let cx = h.clone();
                        pending.approve(
                            target,
                            || Some((InputLeaseId::from_raw(219), InputTicketId::from_raw(223))),
                            move || {
                                calls.fetch_add(1, Ordering::Release);
                                while !unblocked.load(Ordering::Acquire) {
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                                if mode == Mode::FactoryFails {
                                    return Err(PlatformError::Unavailable);
                                }
                                Ok(Sink {
                                    effects,
                                    release_at,
                                    cx,
                                })
                            },
                            |_| true,
                        )?;
                        approval_count += 1;
                        if mode == Mode::CallbackFails {
                            return Err(crate::input_quic::grant::Error::TargetChanged);
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
            ));
            let report = std::future::poll_fn(|task| {
                let polled = service.as_mut().poll(task);
                if mode == Mode::Abandoned && receipts.get() == 1 {
                    assert!(polled.is_pending(), "service ended before abandonment");
                    Poll::Ready(None)
                } else {
                    polled.map(Some)
                }
            })
            .await;
            drop(service);
            view_stop.stop();
            report
        },
        async {
            let result = viewer
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
                                if mode != Mode::NoVisibility
                                    && let Some(p) = pending.presentation()
                                    && shown != Some(p.frame)
                                {
                                    pending.visible(p.frame.as_raw()).unwrap();
                                    shown = Some(p.frame);
                                }
                            }
                            ViewerControlState::Controlled(input) => {
                                granted = true;
                                assert!(matches!(
                                    mode,
                                    Mode::Renewing | Mode::CaptureStall | Mode::Abandoned
                                ));
                                if let Some(p) = frame {
                                    input.visible(p.frame.as_raw()).unwrap();
                                }
                                let at = *since.get_or_insert_with(|| now(&c).unwrap());
                                if !action_sent {
                                    let _ = input.action(key(true)).unwrap();
                                    action_sent = true;
                                }
                                // No release action: final destruction must release
                                // the held key without manufacturing a second receipt.
                                if mode == Mode::Renewing
                                    && receipts.get() == 1
                                    && now(&c).unwrap() >= at + 3_100_000
                                {
                                    stop.revoke();
                                    view_stop.stop();
                                }
                            }
                            ViewerControlState::Observing => panic!("lost control intent"),
                        }
                        Ok(())
                    },
                    |_| receipts.set(receipts.get() + 1),
                )
                .await;
            stop.revoke();
            result
        },
    ))
    .await;
    if mode == Mode::Abandoned {
        assert!(report.is_none());
        assert!(vr.is_err() && stop.check().is_err());
        assert_eq!(receipts.get(), 1);
        asupersync::time::timeout(cleanup.now(), Duration::from_secs(1), async {
            while seat.is_occupied() {
                asupersync::time::sleep(cleanup.now(), Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        {
            let ops = effects.lock().unwrap();
            assert_eq!(ops.len(), 2);
            assert!(matches!(
                ops[0],
                Operation::Key {
                    transition: KeyTransition::Press,
                    ..
                }
            ));
            assert!(matches!(
                ops[1],
                Operation::Key {
                    transition: KeyTransition::Release,
                    ..
                }
            ));
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
        return;
    }
    let report = report.expect("only the abandonment case discards its service future");
    assert!(
        report.session.is_err() && vr.is_err(),
        "{mode:?}: {report:?} {vr:?}"
    );
    assert!(stop.check().is_err());
    if matches!(mode, Mode::Renewing | Mode::CaptureStall | Mode::Abandoned) {
        assert!(granted && action_sent);
        assert_eq!(approval_count, 1);
        assert_eq!(factories.load(Ordering::Acquire), 1);
        assert_eq!(receipts.get(), 1);
        let observed = effects.lock().unwrap();
        assert_eq!(observed.len(), 2, "only press and cleanup release");
        assert!(matches!(
            observed[0],
            Operation::Key {
                transition: KeyTransition::Press,
                ..
            }
        ));
        assert!(matches!(
            observed[1],
            Operation::Key {
                transition: KeyTransition::Release,
                ..
            }
        ));
        assert!(report.input.unwrap().handoff_safe());
        assert!(!seat.is_occupied());
        assert_eq!(host.statistics().encoded_updates, 0);
        assert_eq!(viewer.statistics().decoded, 0);
        if mode == Mode::CaptureStall {
            let at = released.load(Ordering::Acquire);
            assert!(
                at >= start && at < start + 1_000_000,
                "release delayed until capture watchdog"
            );
        } else {
            assert!(host.presentation_reports() > 3);
            assert!(viewer.presentation_reports() > 3);
        }
    } else {
        assert!(!granted && !action_sent);
        assert_eq!(receipts.get(), 0);
        assert!(effects.lock().unwrap().is_empty());
        if matches!(mode, Mode::NoConsent | Mode::NoVisibility) {
            assert_eq!(approval_count, 0);
            assert_eq!(factories.load(Ordering::Acquire), 0);
            assert!(report.input.is_none());
            assert!(!seat.is_occupied());
        } else if mode == Mode::FactoryStall {
            assert_eq!(approval_count, 1);
            assert_eq!(factories.load(Ordering::Acquire), 1);
            let input = report.input.unwrap();
            assert!(input.exit.is_none());
            assert!(!input.handoff_safe());
            assert!(
                seat.is_occupied(),
                "stuck native factory released Seat early"
            );
            gate.0.store(true, Ordering::Release);
            asupersync::time::timeout(cleanup.now(), Duration::from_secs(1), async {
                while seat.is_occupied() {
                    asupersync::time::sleep(cleanup.now(), Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            assert!(effects.lock().unwrap().is_empty());
        } else {
            assert_eq!(approval_count, 1);
            assert!(report.input.unwrap().handoff_safe(), "{report:?}");
            assert!(!seat.is_occupied());
        }
    }
    assert_eq!(host.worker_id(), host_pid);
    assert_eq!(viewer.worker_id(), viewer_pid);
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
fn managed_publication_renews_and_drains_held_input_without_an_external_driver_task() {
    run(|c, h| managed(c, h, Mode::Renewing));
}
#[test]
fn managed_driver_releases_held_key_while_capture_is_stalled() {
    run(|c, h| managed(c, h, Mode::CaptureStall));
}
#[test]
fn managed_service_does_not_approve_without_consent() {
    run(|c, h| managed(c, h, Mode::NoConsent));
}
#[test]
fn managed_service_cannot_replace_visibility_with_decode_or_consent() {
    run(|c, h| managed(c, h, Mode::NoVisibility));
}
#[test]
fn failed_factory_returns_original_native_cleanup_evidence_without_a_grant() {
    run(|c, h| managed(c, h, Mode::FactoryFails));
}
#[test]
fn blocked_factory_returns_bounded_uncertain_shutdown_without_releasing_seat() {
    run(|c, h| managed(c, h, Mode::FactoryStall));
}
#[test]
fn local_failure_after_approval_still_polls_newly_installed_driver_to_cleanup() {
    run(|c, h| managed(c, h, Mode::CallbackFails));
}
#[test]
fn managed_future_is_send_and_unpolled_drop_fences_before_any_local_callback() {
    fn send<T: Send>(_: &T) {}
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let (mut host, mut viewer) = Box::pin(prepare(&c, &h, "unchanged", keys())).await;
        let control = host.control();
        let seat = Seat::default();
        let service = host.serve_managed_control(
            seat.clone(),
            |_| panic!("unpolled consent"),
            || panic!("unpolled credentials"),
            || panic!("unpolled ticket"),
        );
        send(&service);
        drop(service);
        assert!(control.check().is_err());
        assert!(!seat.is_occupied());
        let ManagedControlReport { session, input } = host
            .serve_managed_control(
                seat.clone(),
                |_| panic!("reacquisition"),
                || panic!("nonce"),
                || None,
            )
            .await;
        assert!(session.is_err() && input.is_none());
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
fn dropping_polled_managed_service_releases_held_input_without_returning_a_false_report() {
    run(|c, h| managed(c, h, Mode::Abandoned));
}
