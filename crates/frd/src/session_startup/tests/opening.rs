//! Public host orchestration with actual UDP/TLS and explicit local consent.
use super::*;
use std::sync::atomic::AtomicUsize;

fn assert_send<T: Send>(_: &T) {}
async fn client_until_done(cx: &Cx, viewer: &mut Viewer) {
    while !viewer.startup.is_complete() {
        viewer.drive(cx).await;
    }
}

#[test]
fn public_open_notifies_once_and_transfers_the_original_bound_connection() {
    for approval in [false, true] {
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let (host, mut viewer, alive) = pair(&c, &h, config(approval), true).await;
            let connection = host.transport.as_ref().unwrap().binding();
            let calls = AtomicUsize::new(0);
            let mut escaped = None;
            let opening = host.open(Duration::from_millis(1), |local, role| {
                assert!(approval);
                assert_eq!(role, Role::RequestControl);
                assert_eq!(calls.fetch_add(1, Ordering::AcqRel), 0);
                local.decide(true).unwrap();
                escaped = Some(local);
                Ok(())
            });
            assert_send(&opening);
            let (result, ()) =
                Box::pin(support::both(opening, client_until_done(&c, &mut viewer))).await;
            let mut host = result.unwrap();
            assert_eq!(calls.load(Ordering::Acquire), usize::from(approval));
            assert!(host.io().unwrap().0.is_bound_to(&connection));
            assert!(host.observation().unwrap().check().is_ok());
            if let Some(local) = escaped {
                assert!(local.decide(true).is_err());
            }
            drop(host);
            assert!(!alive.load(Ordering::Acquire));
        });
    }
}

#[test]
fn delayed_local_decision_keeps_notification_single_and_never_implies_consent() {
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (host, mut viewer, alive) = pair(&c, &h, config(true), true).await;
        let slot = std::sync::Mutex::new(None::<Approval>);
        let calls = AtomicUsize::new(0);
        let sent = AtomicBool::new(false);
        let opening = host.open(Duration::from_millis(2), |local, _| {
            calls.fetch_add(1, Ordering::Relaxed);
            *slot.lock().unwrap() = Some(local);
            Ok(())
        });
        let ui = async {
            while slot.lock().unwrap().is_none() {
                asupersync::time::sleep(c.now(), Duration::from_millis(1)).await;
            }
            asupersync::time::sleep(c.now(), Duration::from_millis(40)).await;
            assert_eq!(calls.load(Ordering::Acquire), 1);
            assert!(!sent.load(Ordering::Acquire));
            slot.lock().unwrap().take().unwrap().decide(true).unwrap();
        };
        let client = async {
            while !viewer.startup.is_complete() {
                viewer.drive(&c).await;
                if viewer.routes.inbound.binding != 0 {
                    sent.store(true, Ordering::Release);
                }
            }
        };
        let (result, _) =
            Box::pin(support::both(opening, Box::pin(support::both(ui, client)))).await;
        assert!(result.is_ok());
        assert!(sent.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::Acquire), 1);
        drop(result);
        assert!(!alive.load(Ordering::Acquire));
    });
}

#[test]
fn denial_or_failed_notification_cannot_yield_a_running_session() {
    for denial in [false, true] {
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let (host, mut viewer, alive) = pair(&c, &h, config(true), true).await;
            let notified = AtomicBool::new(false);
            let opening = host.open(Duration::from_millis(1), |local, _| {
                notified.store(true, Ordering::Release);
                if denial {
                    local.decide(false).unwrap();
                    Ok(())
                } else {
                    Err(())
                }
            });
            let client = async {
                while !notified.load(Ordering::Acquire) {
                    viewer.drive(&c).await;
                }
                assert!(!viewer.startup.is_complete());
            };
            let (result, ()) = Box::pin(support::both(opening, client)).await;
            assert!(matches!(result, Err(Error::Denied)));
            assert!(!alive.load(Ordering::Acquire));
            assert!(c.checkpoint().is_ok());
        });
    }
}

#[test]
fn unpolled_open_revokes_before_notification_state_is_destroyed() {
    struct Notice(Arc<AtomicBool>);
    impl Drop for Notice {
        fn drop(&mut self) {
            assert!(
                !self.0.load(Ordering::Acquire),
                "callback dropped with live admission"
            );
        }
    }
    let runtime = support::runtime();
    let c = runtime.request_cx_with_budget(Budget::INFINITE);
    let h = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (host, _viewer, alive) = pair(&c, &h, config(true), true).await;
        let notice = Notice(alive.clone());
        drop(host.open(Duration::from_millis(1), move |_, _| {
            let _held = &notice;
            panic!("unpolled opening must not invoke UI")
        }));
        assert!(!alive.load(Ordering::Acquire));
    });
}

#[test]
fn parked_startup_and_callback_delay_cannot_start_fresh_authority() {
    for parked in [false, true] {
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let mut configuration = config(true);
            configuration.startup_timeout = Duration::from_millis(100);
            let (host, mut viewer, alive) = pair(&c, &h, configuration, true).await;
            let finished = AtomicBool::new(false);
            let opening = host.open(Duration::from_millis(1), |local, _| {
                assert!(!parked);
                std::thread::sleep(Duration::from_millis(120));
                assert_eq!(local.decide(true), Err(Error::Expired));
                Ok(())
            });
            if parked {
                asupersync::time::sleep(h.now(), Duration::from_millis(120)).await;
                assert!(matches!(opening.await, Err(Error::Expired)));
            } else {
                let host = async {
                    let result = opening.await;
                    finished.store(true, Ordering::Release);
                    result
                };
                let client = async {
                    while !finished.load(Ordering::Acquire) {
                        viewer.drive(&c).await;
                    }
                };
                let (result, ()) = Box::pin(support::both(host, client)).await;
                assert!(matches!(result, Err(Error::Expired)));
            }
            assert!(!alive.load(Ordering::Acquire));
        });
    }
}

#[test]
fn invalid_poll_interval_refuses_without_notifying_or_granting() {
    for turn in [Duration::ZERO, Duration::from_millis(101)] {
        let runtime = support::runtime();
        let c = runtime.request_cx_with_budget(Budget::INFINITE);
        let h = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let (host, _viewer, alive) = pair(&c, &h, config(true), true).await;
            assert!(matches!(
                host.open(turn, |_, _| panic!("invalid open")).await,
                Err(Error::InvalidConfiguration)
            ));
            assert!(!alive.load(Ordering::Acquire));
        });
    }
}
