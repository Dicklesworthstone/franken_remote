//! Real TLS/UDP and supervised child IPC. Local permission, monitor/HEVC and
//! decoder acknowledgements are fixtures, not installed-tailnet/hardware proof.
use super::super::super::super::preparation;
use super::*;
use crate::session_agent::source::desktop::dispatch::{Driver, Error as DispatchError, Incoming};

fn pair(rt: &Runtime) -> (Incoming, Driver, Cx) {
    let cx = rt.request_cx_with_budget(Budget::INFINITE);
    let (incoming, driver) = agent()
        .native_incoming(
            cx.clone(),
            service::Policy::default(),
            Duration::from_millis(50),
            entropy(),
        )
        .unwrap();
    (incoming, driver, cx)
}
async fn reap_driver(driver: &mut Driver, retirement: &mut crate::worker::Retirement) {
    let cx = Cx::current().unwrap();
    assert!(
        driver
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        retirement
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap()
            .is_some()
    );
}
#[test]
#[allow(clippy::too_many_lines)]
fn native_dispatch_cold_consent_then_warm_sibling_survives_first_connection_drop() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let (incoming, mut driver, independent) = pair(&rt);
        let Fresh {
            c,
            h,
            host,
            mut viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(4)).await;
        let notice = Arc::new(Mutex::new(None));
        let saved = notice.clone();
        let first_connection = incoming
            .serve_host(host, move |a, _| {
                *saved.lock().unwrap() = Some(a);
                Ok(())
            })
            .unwrap();
        let Fresh {
            host: extra,
            h: refused,
            viewer: _unused,
            ..
        } = *fresh(&rt, 14, false, Role::Observe, Duration::from_secs(4)).await;
        assert!(matches!(
            incoming.serve_host(extra, |_, _| Ok(())),
            Err(DispatchError::Busy)
        ));
        assert!(refused.is_cancel_requested());
        let (setup, source, mut retirement, trace) = preparation::setup(&rt, "normal");
        let factories = Arc::new(AtomicU64::new(0));
        let counted = factories.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut running = Box::pin(driver.serve(
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(setup)
            },
            preparation::choose,
            move |_, _| {
                Ok(if local_stop.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
        ));
        let mut clients = Box::pin(async {
            prompt(&mut viewer, &notice).await;
            assert_eq!(factories.load(Ordering::SeqCst), 0);
            assert!(
                !trace.exists(),
                "no child/discovery before actual first consent"
            );
            notice.lock().unwrap().take().unwrap().decide(true).unwrap();
            let mut first = Box::pin(Client::start(c, viewer)).await;
            first.ready().await;
            assert_eq!(factories.load(Ordering::SeqCst), 1);
            let Fresh {
                c, host, viewer, ..
            } = *fresh(&rt, 15, false, Role::Observe, Duration::from_secs(4)).await;
            let second_connection = incoming
                .serve_host(host, |_, _| panic!("unattended sibling"))
                .unwrap();
            let mut second = {
                let mut keep = Box::pin(async {
                    loop {
                        first.turn().await;
                    }
                });
                let mut next = Box::pin(async {
                    let mut p = Box::pin(Client::start(c.clone(), viewer)).await;
                    p.ready().await;
                    p
                });
                poll_fn(|task| {
                    assert!(keep.as_mut().poll(task).is_pending());
                    next.as_mut().poll(task)
                })
                .await
            };
            // The cold reply need not have been polled by its connection task:
            // dropping that future still closes precisely its original ticket.
            drop(first_connection);
            first.viewer.close();
            assert!(h.is_cancel_requested());
            assert!(!independent.is_cancel_requested());
            let until = now(&c).unwrap() + 3_100_000;
            while now(&c).unwrap() < until {
                second.turn().await;
            }
            assert!(source.check().is_ok());
            assert!(!second.frames.is_empty());
            assert_eq!(
                factories.load(Ordering::SeqCst),
                1,
                "no replacement source on first departure"
            );
            stop.store(true, Ordering::Release);
            // Keep the second connection scope alive until local stop retires it.
            std::future::pending::<()>().await;
            drop(second_connection);
        });
        let report = poll_fn(|task| {
            if let Poll::Ready(result) = running.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(clients.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert_eq!(report.viewers.admitted, 2);
        assert!(report.source_renewals >= 3);
        assert!(source.check().is_err());
        drop(clients);
        drop(running);
        assert!(driver.worker_id().is_some());
        reap_driver(&mut driver, &mut retirement).await;
    }));
}
#[test]
fn native_dispatch_cold_queue_uses_original_deadline_without_polling_the_source_driver() {
    let rt = support::runtime();
    rt.block_on(async {
        let (incoming, driver, independent) = pair(&rt);
        let Fresh {
            host,
            h,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_millis(80)).await;
        let peer = incoming
            .serve_host(host, |_, _| panic!("no driver, no approval notification"))
            .unwrap();
        let result =
            asupersync::time::timeout(Cx::current().unwrap().now(), Duration::from_secs(1), peer)
                .await
                .unwrap();
        assert_eq!(result, Err(DispatchError::Startup(OpenError::Expired)));
        assert!(h.is_cancel_requested());
        assert!(!independent.is_cancel_requested());
        assert!(driver.worker_id().is_none());
        drop(driver);
        let Fresh {
            host,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 14, false, Role::Observe, Duration::from_secs(1)).await;
        assert!(matches!(
            incoming.serve_host(host, |_, _| Ok(())),
            Err(DispatchError::Closed)
        ));
    });
}
#[test]
fn native_dispatch_unpolled_driver_drop_fences_queued_host_without_factory_or_local_events() {
    let rt = support::runtime();
    rt.block_on(async {
        let (incoming, mut driver, _) = pair(&rt);
        let Fresh {
            host,
            h,
            alive,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
        let peer = incoming
            .serve_host(host, |_, _| panic!("no notification"))
            .unwrap();
        drop(driver.serve(
            || panic!("no factory"),
            preparation::choose,
            |_, _| panic!("unpolled local"),
        ));
        assert!(h.is_cancel_requested());
        assert!(!alive.load(Ordering::Acquire));
        assert!(peer.await.is_err());
        assert!(driver.worker_id().is_none());
    });
}
#[test]
fn native_dispatch_local_revoke_or_panic_fences_queued_host_with_failed_future_retained() {
    let rt = support::runtime();
    rt.block_on(async {
        for panic in [false, true] {
            let (incoming, mut driver, independent) = pair(&rt);
            let Fresh {
                host,
                h,
                alive,
                viewer: _viewer,
                ..
            } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
            let _peer = incoming
                .serve_host(host, |_, _| panic!("no notification"))
                .unwrap();
            let mut run = Box::pin(driver.serve(
                || panic!("no factory"),
                preparation::choose,
                move |agent, _| {
                    assert!(!agent.is_revoked());
                    assert!(!panic, "local event fixture");
                    Ok(LocalAction::Stop)
                },
            ));
            poll_fn(|task| {
                let result = catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(task)));
                if panic {
                    assert!(result.is_err());
                } else {
                    assert!(matches!(result.unwrap(), Poll::Ready(Err(_))));
                }
                assert!(h.is_cancel_requested());
                assert!(!independent.is_cancel_requested());
                Poll::Ready(())
            })
            .await;
            drop(run);
            assert!(!alive.load(Ordering::Acquire));
        }
    });
}
#[test]
fn native_dispatch_independent_cancellation_while_first_approval_waits_starts_no_source() {
    let rt = support::runtime();
    rt.block_on(async {
        let (incoming, mut driver, independent) = pair(&rt);
        let Fresh {
            host,
            h,
            mut viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(3)).await;
        let notice = Arc::new(Mutex::new(None));
        let saved = notice.clone();
        let peer = incoming
            .serve_host(host, move |a, _| {
                *saved.lock().unwrap() = Some(a);
                Ok(())
            })
            .unwrap();
        let mut run = Box::pin(driver.serve(
            || panic!("consent was never granted"),
            preparation::choose,
            |_, _| Ok(LocalAction::Continue),
        ));
        let mut client = Box::pin(async {
            prompt(&mut viewer, &notice).await;
            independent.cancel_fast(asupersync::types::CancelKind::User);
            std::future::pending::<()>().await;
        });
        let result = poll_fn(|task| {
            if let Poll::Ready(result) = run.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        assert!(result.is_err());
        assert!(h.is_cancel_requested());
        drop(client);
        drop(run);
        assert!(peer.await.is_err());
        assert!(driver.worker_id().is_none());
        assert!(
            notice.lock().unwrap().take().unwrap().decide(true).is_err(),
            "late approval cannot reopen"
        );
    });
}
#[test]
fn native_dispatch_rejects_foreign_os_session_without_consuming_cold_slot() {
    let rt = support::runtime();
    rt.block_on(async {
        let (incoming, _driver, _) = pair(&rt);
        let Fresh {
            mut host,
            h,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
        host.config.binding.os_session = OsSessionId::from_raw(99);
        assert!(matches!(
            incoming.serve_host(host, |_, _| Ok(())),
            Err(DispatchError::WrongSession)
        ));
        assert!(h.is_cancel_requested());
        let Fresh {
            host,
            h,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 14, false, Role::Observe, Duration::from_secs(2)).await;
        let peer = incoming.serve_host(host, |_, _| Ok(())).unwrap();
        drop(peer);
        assert!(h.is_cancel_requested());
    });
}

#[test]
fn incoming_dispatch_awaits_local_source_setup_on_the_independent_driver() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let (incoming, mut driver, independent) = pair(&rt);
        let Fresh {
            c, h, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(3)).await;
        let connection = incoming
            .serve_host(host, |_, _| panic!("unattended"))
            .unwrap();
        let (setup, source, mut retirement, trace) = preparation::setup(&rt, "normal");
        let ready = Arc::new(AtomicBool::new(false));
        let prepared = ready.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let clock = independent.clone();
        let mut operation = Box::pin(driver.serve_async(
            move || async move {
                assert!(!trace.exists(), "no worker before async setup");
                asupersync::time::sleep(clock.now(), Duration::from_millis(50)).await;
                assert!(!trace.exists(), "setup wait must not launch native work");
                prepared.store(true, Ordering::Release);
                Ok(setup)
            },
            preparation::choose,
            move |_, _| {
                Ok(if local_stop.load(Ordering::Acquire) {
                    LocalAction::Stop
                } else {
                    LocalAction::Continue
                })
            },
        ));
        is_send(&operation);
        let mut client = Box::pin(async {
            let mut first = Box::pin(Client::start(c, viewer)).await;
            first.ready().await;
            assert!(ready.load(Ordering::Acquire));
            assert!(!first.frames.is_empty());
            assert!(!independent.is_cancel_requested());
            assert!(!h.is_cancel_requested());
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
            drop(connection);
        });
        let result = poll_fn(|task| {
            if let Poll::Ready(result) = operation.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert_eq!(result.viewers.admitted, 1);
        assert!(source.check().is_err());
        drop(client);
        drop(operation);
        assert!(
            !independent.is_cancel_requested(),
            "local source stop must not cancel its independent runtime context"
        );
        assert!(h.is_cancel_requested());
        reap_driver(&mut driver, &mut retirement).await;
    }));
}
