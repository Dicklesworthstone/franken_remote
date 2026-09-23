//! Real UDP/TLS and worker IPC; permission, screen and codec data are fixtures.
use super::*;
use std::sync::atomic::AtomicUsize;
struct Dropped(Arc<AtomicBool>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn async_source_wait_renews_the_original_peer_before_native_start() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let Fresh {
            c, h, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(6)).await;
        let receipt = Receipt::default();
        let copy = receipt.clone();
        let events = Arc::new(AtomicUsize::new(0));
        let counted = events.clone();
        let mut agent = preparation::agent();
        let clock = h.clone();
        let setup_ready = Arc::new(AtomicBool::new(false));
        let ready = setup_ready.clone();
        let opening = agent
            .open_native_shared_desktop_async(
                host,
                || async {
                    // Longer than the three-second observation lease. The original
                    // peer must keep renewing while no source or media exists.
                    asupersync::time::sleep(clock.now(), Duration::from_millis(3150)).await;
                    assert!(clock.checkpoint().is_ok());
                    assert!(copy.lock().unwrap().is_none());
                    let setup = factory(&rt, copy, "normal")()?;
                    ready.store(true, Ordering::Release);
                    Ok(setup)
                },
                preparation::choose,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("unattended"),
                move |_, _| {
                    counted.fetch_add(1, Ordering::Relaxed);
                    Ok(LocalAction::Continue)
                },
            )
            .unwrap();
        fn send<T: Send>(_: &T) {}
        send(&opening);
        let (result, mut client) = support::both(
            opening,
            Client::start_when(c, viewer, || setup_ready.load(Ordering::Acquire)),
        )
        .await;
        let mut desktop = result.unwrap();
        let _original_child = desktop.worker_id();
        assert!(h.checkpoint().is_ok());
        assert!(events.load(Ordering::Relaxed) > 20);
        client.viewer.close();
        desktop.close();
        drop(desktop);
        cleanup(&receipt, true).await;
    }));
}

#[test]
fn pending_async_source_obeys_original_deadline_and_local_revoke() {
    for revoke in [false, true] {
        let rt = support::runtime();
        rt.block_on(async {
            let Fresh {
                h, host, viewer, ..
            } = *fresh(&rt, 13, false, Role::Observe, Duration::from_millis(250)).await;
            let entered = Arc::new(AtomicBool::new(false));
            let inside = entered.clone();
            let dropped = Arc::new(AtomicBool::new(false));
            let notice = Dropped(dropped.clone());
            let mut agent = preparation::agent();
            let run = agent
                .open_native_shared_desktop_async(
                    host,
                    move || async move {
                        let _notice = notice;
                        inside.store(true, Ordering::Release);
                        std::future::pending::<Result<Setup, ()>>().await
                    },
                    |_| panic!("pending factory cannot discover"),
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("unattended"),
                    |agent, _| {
                        if revoke && entered.load(Ordering::Acquire) {
                            agent.permissions_mut().set_permission(
                                PermissionKind::ScreenCapture,
                                PermissionStatus::Denied,
                            );
                        }
                        Ok(LocalAction::Continue)
                    },
                )
                .unwrap();
            let error = refused(Box::pin(run), viewer).await;
            assert!(entered.load(Ordering::Acquire));
            assert!(dropped.load(Ordering::Acquire));
            assert!(h.checkpoint().is_err());
            if revoke {
                assert!(matches!(error, DesktopError::Consent(_)), "{error:?}");
            } else {
                assert!(
                    matches!(error, DesktopError::Startup(OpenError::Expired)),
                    "{error:?}"
                );
            }
        });
    }
}

#[test]
fn asynchronous_factory_is_not_invoked_before_actual_approval() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            h, host, viewer, ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
        let invoked = Arc::new(AtomicBool::new(false));
        let inside = invoked.clone();
        let decided = Arc::new(AtomicBool::new(false));
        let decision = decided.clone();
        let observed = invoked.clone();
        let mut agent = preparation::agent();
        let operation = agent
            .open_native_shared_desktop_async(
                host,
                move || {
                    inside.store(true, Ordering::Release);
                    std::future::ready(Err(()))
                },
                |_| panic!("denied"),
                service::Policy::default(),
                entropy(),
                move |approval, _| {
                    assert!(!observed.load(Ordering::Acquire));
                    approval.decide(false).unwrap();
                    decision.store(true, Ordering::Release);
                    Ok(())
                },
                local,
            )
            .unwrap();
        let error = refused(Box::pin(operation), viewer).await;
        assert!(decided.load(Ordering::Acquire));
        assert!(
            matches!(error, DesktopError::Startup(OpenError::Denied)),
            "{error:?}"
        );
        assert!(!invoked.load(Ordering::Acquire));
        assert!(h.checkpoint().is_err());
    });
}

#[test]
fn async_factory_panic_fences_peer_and_drops_pending_native_owner() {
    let rt = support::runtime();
    let cancelled = Arc::new(Mutex::new(None));
    let dropped = Arc::new(AtomicBool::new(false));
    let notice = Dropped(dropped.clone());
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(async {
            let Fresh {
                h, host, viewer, ..
            } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
            *cancelled.lock().unwrap() = Some(h);
            let mut agent = preparation::agent();
            let operation = agent
                .open_native_shared_desktop_async(
                    host,
                    move || async move {
                        let _notice = notice;
                        asupersync::runtime::yield_now().await;
                        panic!("local native initializer failed")
                    },
                    |_| panic!("never select"),
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("unattended"),
                    local,
                )
                .unwrap();
            let _ = refused(Box::pin(operation), viewer).await;
        })
    }));
    assert!(caught.is_err());
    assert!(dropped.load(Ordering::Acquire));
    assert!(
        cancelled
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .checkpoint()
            .is_err()
    );
}
