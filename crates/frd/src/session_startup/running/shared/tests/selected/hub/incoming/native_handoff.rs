//! The original local source moves out of the first connection's callback;
//! neither a callback return nor a retained ticket silently owns its lifetime.
use super::super::super::super::preparation;
use super::*;
use crate::session_agent::source::desktop::NativeDesktop;

async fn opened(
    rt: &Runtime,
) -> (
    SessionAgent,
    NativeDesktop,
    Box<Client>,
    ObservationControl,
    crate::worker::Retirement,
) {
    let Fresh {
        c, host, viewer, ..
    } = *fresh(rt, 13, false, Role::Observe, Duration::from_secs(3)).await;
    let mut agent = super::agent();
    let (setup, source, retirement, _) = preparation::setup(rt, "normal");
    let (desktop, client) = Box::pin(support::both(
        agent
            .open_native_shared_desktop(
                host,
                || Ok(setup),
                preparation::choose,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("unattended fixture"),
                |_, _| Ok(LocalAction::Continue),
            )
            .unwrap(),
        Client::start(c, viewer),
    ))
    .await;
    (agent, desktop.unwrap(), client, source, retirement)
}
async fn reap_desktop(desktop: &mut NativeDesktop, retirement: &mut crate::worker::Retirement) {
    let cx = Cx::current().unwrap();
    desktop
        .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
        .await
        .unwrap();
    assert!(
        retirement
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One continuous source/peer ownership lifecycle.
fn first_native_handoff_retains_peer_scope_without_owning_sibling_capture() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let (agent, desktop, mut first, source, mut retirement) = opened(&rt).await;
        let pid = desktop.worker_id();
        let delivered = Arc::new(Mutex::new(None));
        let saved = delivered.clone();
        let mut connection = Box::pin(
            desktop
                .handoff(agent, move |agent, desktop| {
                    let mut slot = saved.lock().unwrap();
                    assert!(slot.is_none());
                    *slot = Some((agent, desktop));
                    Ok(())
                })
                .unwrap(),
        );
        let (mut agent, mut desktop) = delivered.lock().unwrap().take().unwrap();
        let first_ticket = connection.ticket();
        let admission = desktop.admissions();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut running = Box::pin(
            desktop
                .serve(
                    &mut agent,
                    Duration::from_millis(50),
                    entropy(),
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
        let mut clients = Box::pin(async {
            first.ready().await;
            let Fresh {
                c, host, viewer, ..
            } = *fresh(&rt, 14, false, Role::Observe, Duration::from_secs(4)).await;
            let second_ticket = admission
                .admit_host(host, |_, _| panic!("unattended late peer"))
                .unwrap();
            let mut second = {
                let mut keep = Box::pin(async {
                    loop {
                        first.turn().await;
                    }
                });
                let mut next = Box::pin(async {
                    let mut peer = Box::pin(Client::start(c.clone(), viewer)).await;
                    peer.ready().await;
                    peer
                });
                poll_fn(|task| {
                    assert!(keep.as_mut().poll(task).is_pending());
                    next.as_mut().poll(task)
                })
                .await
            };
            assert_eq!(first_ticket.state(), State::Serving);
            first_ticket.close();
            first.viewer.close();
            let until = now(&c).unwrap() + 3_100_000;
            while now(&c).unwrap() < until {
                second.turn().await;
            }
            assert_eq!(second_ticket.state(), State::Serving);
            assert!(source.check().is_ok());
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        let mut result = None;
        let report = poll_fn(|task| {
            if let Poll::Ready(report) = running.as_mut().poll(task) {
                return Poll::Ready(report);
            }
            if result.is_none()
                && let Poll::Ready(value) = connection.as_mut().poll(task)
            {
                result = Some(value);
            }
            assert!(clients.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert_eq!(result, Some(Err(service::Error::Closed)));
        assert_eq!(
            connection.as_mut().await,
            result.unwrap(),
            "retained completed scope keeps exact receipt"
        );
        assert_eq!(report.viewers.admitted, 2);
        assert!(report.source_renewals >= 3);
        drop(clients);
        drop(running);
        drop(connection);
        assert_eq!(desktop.worker_id(), pid);
        reap_desktop(&mut desktop, &mut retirement).await;
    }));
}

#[test]
fn native_handoff_refusal_panic_and_reentrant_revoke_fence_already_stored_cohort() {
    let rt = support::runtime();
    rt.block_on(async {
        for mode in 0..3 {
            let (agent, desktop, _client, source, mut retirement) = opened(&rt).await;
            let Fresh {
                host,
                viewer: _pending_peer,
                ..
            } = *fresh(&rt, 14, true, Role::Observe, Duration::from_secs(3)).await;
            let initial = desktop.first();
            let stored = Arc::new(Mutex::new(None));
            let saved = stored.clone();
            let pending = Arc::new(Mutex::new(None));
            let ticket = pending.clone();
            let observed = source.clone();
            let result = catch_unwind(AssertUnwindSafe(|| {
                desktop.handoff(agent, move |agent, desktop| {
                    *ticket.lock().unwrap() = Some(
                        desktop
                            .admissions()
                            .admit_host(host, |_, _| panic!("pending notification"))
                            .unwrap(),
                    );
                    *saved.lock().unwrap() = Some((agent, desktop));
                    match mode {
                        0 => Err(()),
                        1 => panic!("local publication failure"),
                        _ => {
                            observed.revoke();
                            Ok(())
                        }
                    }
                })
            }));
            if mode == 1 {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().is_err());
            }
            assert!(source.check().is_err());
            assert!(matches!(initial.state(), State::Finished(Err(_))));
            assert!(matches!(
                pending.lock().unwrap().as_ref().unwrap().state(),
                State::Finished(Err(_))
            ));
            let (_agent, mut desktop) = stored.lock().unwrap().take().unwrap();
            reap_desktop(&mut desktop, &mut retirement).await;
        }
    });
}

#[test]
fn dropping_initial_connection_service_before_polling_fences_only_that_viewer() {
    let rt = support::runtime();
    rt.block_on(async {
        let (agent, desktop, _client, source, mut retirement) = opened(&rt).await;
        let ticket = desktop.first();
        let mut pair = None;
        let scope = desktop
            .handoff(agent, |agent, desktop| {
                pair = Some((agent, desktop));
                Ok(())
            })
            .unwrap();
        drop(scope);
        assert_eq!(ticket.state(), State::Finished(Err(service::Error::Closed)));
        assert!(
            source.check().is_ok(),
            "first connection is not source consent"
        );
        let (_agent, mut desktop) = pair.unwrap();
        reap_desktop(&mut desktop, &mut retirement).await;
    });
}

#[test]
fn connection_service_keeps_the_original_pending_deadline_and_terminal_receipt() {
    let rt = support::runtime();
    rt.block_on(async {
        let (_agent, mut desktop, _client, source, mut retirement) = opened(&rt).await;
        let Fresh {
            host,
            viewer: _pending,
            ..
        } = *fresh(&rt, 14, true, Role::Observe, Duration::from_millis(150)).await;
        let pending = desktop
            .admissions()
            .serve_host(host, |_, _| {
                panic!("receipt watcher does not drive or approve the peer")
            })
            .unwrap();
        let ticket = pending.ticket();
        assert_eq!(ticket.state(), State::Opening);
        // No hub/packet traffic drives completion. The existing Host deadline and
        // the receipt's original timer domain must still make expiry observable.
        let result = asupersync::time::timeout(
            Cx::current().unwrap().now(),
            Duration::from_secs(1),
            pending,
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            Err(service::Error::Session(
                crate::session_startup::Error::Expired
            ))
        );
        assert_eq!(ticket.state(), State::Finished(result));
        // Rewrapping the authentic finished ticket cannot reset that outcome,
        // renew authority or address a replacement through the old numeric ID.
        assert_eq!(ticket.clone().into_service().unwrap().await, result);
        assert!(
            source.check().is_ok(),
            "pending peer is not source authority"
        );
        reap_desktop(&mut desktop, &mut retirement).await;
    });
}
