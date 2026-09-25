//! The first Host is never pre-opened by the fixture. Native source/permission
//! replies remain explicit fixtures; the original TLS/UDP owners are real.
use super::*;
use crate::session_agent::source::desktop::{Error as DesktopError, LocalAction};

#[test]
#[allow(clippy::too_many_lines)]
fn first_observer_bootstrap_services_approval_then_hands_original_owners_to_desktop() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            c,
            h,
            host,
            mut viewer,
            alive,
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let pid = publisher.worker_id();
        let mut agent = super::agent();
        let prompt_slot = Arc::new(Mutex::new(None));
        let notify_slot = prompt_slot.clone();
        let events = Arc::new(AtomicU64::new(0));
        let event_count = events.clone();
        let random = entropy();
        let opening = agent
            .open_shared_desktop(
                host,
                &mut publisher,
                &initial,
                service::Policy::default(),
                random.clone(),
                move |a, role| {
                    assert_eq!(role, Role::Observe);
                    *notify_slot.lock().unwrap() = Some(a);
                    Ok(())
                },
                move |_, _| {
                    event_count.fetch_add(1, Ordering::Relaxed);
                    Ok(LocalAction::Continue)
                },
            )
            .unwrap();
        is_send(&opening);
        let (hub, mut client) = Box::pin(support::both(opening, async {
            prompt(&mut viewer, &prompt_slot).await;
            let before = events.load(Ordering::Relaxed);
            for _ in 0..6 {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            assert!(
                !viewer.is_complete(),
                "no observation while approval is parked"
            );
            assert!(
                events.load(Ordering::Relaxed) > before,
                "local events continue during approval"
            );
            prompt_slot
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .decide(true)
                .unwrap();
            Box::pin(Client::start(c, viewer)).await
        }))
        .await;
        let mut hub = hub.unwrap();
        let ticket = hub.initial();
        assert_eq!(ticket.state(), State::Serving);
        assert!(alive.load(Ordering::Acquire));
        assert!(h.checkpoint().is_ok());
        assert!(!owner.view_ready().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut service = Box::pin(
            agent
                .serve_shared_desktop(
                    &mut publisher,
                    &mut hub,
                    Duration::from_millis(50),
                    random,
                    move |_, _| {
                        Ok(if local_stop.load(Ordering::Acquire) {
                            LocalAction::Stop
                        } else {
                            LocalAction::Continue
                        })
                    },
                )
                .unwrap(),
        );
        let mut peer = Box::pin(async {
            client.ready().await;
            let until = now(&h).unwrap() + 3_100_000;
            while now(&h).unwrap() < until {
                client.turn().await;
            }
            assert_eq!(ticket.state(), State::Serving);
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        let report = poll_fn(|task| {
            if let Poll::Ready(result) = service.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(peer.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert!(report.source_renewals >= 3);
        assert_eq!(report.viewers.admitted, 1);
        assert!(owner.check().is_err());
        assert!(matches!(ticket.state(), State::Finished(Err(_))));
        drop(peer);
        drop(service);
        assert_eq!(publisher.worker_id(), pid);
        reap(&mut publisher).await;
    });
}

#[test]
fn first_observer_unpolled_bootstrap_revokes_both_original_owners() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            host,
            h,
            alive,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut agent = super::agent();
        let opening = agent
            .open_shared_desktop(
                host,
                &mut publisher,
                &initial,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("unpolled notification"),
                |_, _| panic!("unpolled local event"),
            )
            .unwrap();
        drop(opening);
        assert!(owner.check().is_err());
        assert!(h.checkpoint().is_err());
        assert!(!alive.load(Ordering::Acquire));
        reap(&mut publisher).await;
    });
}

#[test]
fn first_observer_local_stop_and_permission_loss_precede_network_and_notification() {
    let rt = support::runtime();
    rt.block_on(async {
        for revoked in [false, true] {
            let Fresh {
                host,
                h,
                alive,
                viewer: _viewer,
                ..
            } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
            let (mut publisher, owner, initial) = selected_publisher(&rt).await;
            let mut agent = super::agent();
            let control = owner.clone();
            let mut opening = Box::pin(
                agent
                    .open_shared_desktop(
                        host,
                        &mut publisher,
                        &initial,
                        service::Policy::default(),
                        entropy(),
                        |_, _| panic!("no approval work after local stop"),
                        move |agent, _| {
                            if revoked {
                                let (_, releases) =
                                    agent.on_screen_capture_revoked(control.check().unwrap());
                                assert!(releases.is_empty(), "observation never granted input");
                                Ok(LocalAction::Continue)
                            } else {
                                Ok(LocalAction::Stop)
                            }
                        },
                    )
                    .unwrap(),
            );
            assert!(opening.as_mut().await.is_err());
            assert!(
                owner.check().is_err(),
                "terminal result fences before future drop"
            );
            assert!(h.checkpoint().is_err());
            assert!(!alive.load(Ordering::Acquire));
            drop(opening);
            reap(&mut publisher).await;
        }
    });
}

#[test]
fn first_observer_refuses_control_intent_without_prompt_or_downgrade() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            host,
            mut viewer,
            h,
            ..
        } = *fresh(&rt, 13, true, Role::RequestControl, Duration::from_secs(2)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut agent = super::agent();
        let mut opening = Box::pin(
            agent
                .open_shared_desktop(
                    host,
                    &mut publisher,
                    &initial,
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("control intent must refuse before approval"),
                    |_, _| Ok(LocalAction::Continue),
                )
                .unwrap(),
        );
        let mut client = Box::pin(async {
            // A terminal viewer returns its error before awaiting anything, so
            // driving it again would spin this poll forever.
            while viewer.drive(Duration::from_millis(1)).await.is_ok() {}
            std::future::pending::<()>().await;
        });
        let result = poll_fn(|task| {
            if let Poll::Ready(result) = opening.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        assert!(result.is_err());
        assert!(owner.check().is_err());
        assert!(h.checkpoint().is_err());
        drop(client);
        drop(opening);
        reap(&mut publisher).await;
    });
}

#[test]
fn first_observer_unused_source_budget_is_not_renewed_by_local_service() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            host,
            mut viewer,
            h,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(4)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut agent = super::agent();
        let calls = Arc::new(AtomicU64::new(0));
        let count = calls.clone();
        let held = Arc::new(Mutex::new(None));
        let save = held.clone();
        let start = now(&h).unwrap();
        let mut opening = Box::pin(
            agent
                .open_shared_desktop(
                    host,
                    &mut publisher,
                    &initial,
                    service::Policy::default(),
                    entropy(),
                    move |a, _| {
                        *save.lock().unwrap() = Some(a);
                        Ok(())
                    },
                    move |_, _| {
                        count.fetch_add(1, Ordering::Relaxed);
                        Ok(LocalAction::Continue)
                    },
                )
                .unwrap(),
        );
        let mut client = Box::pin(async {
            // A terminal viewer returns its error before awaiting anything, so
            // driving it again would spin this poll forever.
            while viewer.drive(Duration::from_millis(1)).await.is_ok() {}
            std::future::pending::<()>().await;
        });
        let result = poll_fn(|task| {
            if let Poll::Ready(result) = opening.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        assert!(result.is_err());
        assert!(calls.load(Ordering::Relaxed) > 2);
        let elapsed = owner.context().timer_driver().unwrap().now().as_nanos() / 1000 - start;
        assert!(
            (1_500_000..2_500_000).contains(&elapsed),
            "unused source deadline, not the four-second peer budget: {elapsed}"
        );
        assert!(owner.check().is_err());
        assert!(h.checkpoint().is_err());
        assert!(held.lock().unwrap().take().unwrap().decide(true).is_err());
        drop(client);
        drop(opening);
        reap(&mut publisher).await;
    });
}

#[test]
fn first_observer_caught_local_panic_fences_before_retained_future_is_dropped() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            host,
            h,
            alive,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut agent = super::agent();
        let mut opening = Box::pin(
            agent
                .open_shared_desktop(
                    host,
                    &mut publisher,
                    &initial,
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("network must not be reached"),
                    |_, _| panic!("local adapter fault"),
                )
                .unwrap(),
        );
        poll_fn(|task| {
            assert!(catch_unwind(AssertUnwindSafe(|| opening.as_mut().poll(task))).is_err());
            Poll::Ready(())
        })
        .await;
        assert!(owner.check().is_err());
        assert!(h.checkpoint().is_err());
        assert!(!alive.load(Ordering::Acquire));
        assert!(matches!(opening.as_mut().await, Err(DesktopError::Closed)));
        drop(opening);
        reap(&mut publisher).await;
    });
}

#[test]
fn first_observer_foreign_agent_cannot_take_or_revoke_original_source_owner() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            host,
            h,
            alive,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut original = super::agent();
        original.attach_shared_source(&publisher).unwrap();
        let mut foreign = super::agent();
        let result = foreign.open_shared_desktop(
            host,
            &mut publisher,
            &initial,
            service::Policy::default(),
            entropy(),
            |_, _| panic!("foreign source"),
            |_, _| panic!("foreign source"),
        );
        assert!(result.is_err());
        drop(result);
        assert!(owner.check().is_ok());
        assert!(h.checkpoint().is_err());
        assert!(!alive.load(Ordering::Acquire));
        drop(original);
        assert!(owner.check().is_err());
        reap(&mut publisher).await;
    });
}
