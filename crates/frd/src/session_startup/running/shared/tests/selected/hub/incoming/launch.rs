//! Complete first-admission -> running-service path, never manually handing a Hub.
use super::*;
use crate::session_agent::source::desktop::Error as DesktopError;

async fn consume<T>(client: &mut Client, work: impl Future<Output = T>) -> T {
    let mut service = Box::pin(async {
        loop {
            client.turn().await;
        }
    });
    let mut work = Box::pin(work);
    poll_fn(|task| {
        assert!(service.as_mut().poll(task).is_pending());
        work.as_mut().poll(task)
    })
    .await
}

#[test]
#[allow(clippy::too_many_lines)]
fn desktop_launch_continues_from_first_admission_through_late_join_and_first_departure() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let first = fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
        let second = fresh(&rt, 14, false, Role::Observe, Duration::from_secs(4)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let pid = publisher.worker_id();
        let Fresh {
            c, host, viewer, ..
        } = *first;
        let Fresh {
            c: c2,
            host: second_host,
            viewer: second_viewer,
            ..
        } = *second;
        let mut agent = super::agent();
        let advertised = Arc::new(Mutex::new(None));
        let saved = advertised.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let announced = Arc::new(AtomicU64::new(0));
        let count = announced.clone();
        let observed = owner.clone();
        let mut run = Box::pin(
            agent
                .run_shared_desktop(
                    host,
                    &mut publisher,
                    &initial,
                    service::Policy::default(),
                    Duration::from_millis(50),
                    entropy(),
                    |_, _| panic!("unattended first host must not request approval"),
                    move |_, _| {
                        Ok(if local_stop.load(Ordering::Acquire) {
                            LocalAction::Stop
                        } else {
                            LocalAction::Continue
                        })
                    },
                    move |admission, ticket| {
                        assert_eq!(count.fetch_add(1, Ordering::Relaxed), 0);
                        assert_eq!(ticket.state(), State::Serving);
                        assert!(
                            !observed.view_ready().unwrap(),
                            "publication is not visibility"
                        );
                        *saved.lock().unwrap() = Some((admission, ticket));
                        Ok(())
                    },
                )
                .unwrap(),
        );
        is_send(&run);
        let mut peers = Box::pin(async {
            let mut first = Box::pin(Client::start(c, viewer)).await;
            first.ready().await;
            let (admission, first_ticket) = advertised.lock().unwrap().take().unwrap();
            assert_eq!(admission.statistics().unwrap().admitted, 1);
            let second_ticket = admission
                .admit_host(second_host, |_, _| panic!("unattended late viewer"))
                .unwrap();
            let mut second = consume(&mut first, async {
                let mut second = Box::pin(Client::start(c2.clone(), second_viewer)).await;
                second.ready().await;
                second
            })
            .await;
            assert_eq!(second_ticket.state(), State::Serving);
            first_ticket.close();
            first.viewer.close();
            let until = now(&c2).unwrap() + 3_100_000;
            // The clock below is the still-live second peer's original clock;
            // first Host cancellation must not own the entire desktop service.
            while crate::media::host_now(&c2).unwrap().as_micros() < until {
                second.turn().await;
            }
            assert!(matches!(first_ticket.state(), State::Finished(Err(_))));
            assert_eq!(second_ticket.state(), State::Serving);
            assert!(owner.check().is_ok());
            assert_ne!(second.frames, [] as [u64; 0]);
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        let report = poll_fn(|task| {
            if let Poll::Ready(result) = run.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(peers.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(announced.load(Ordering::Relaxed), 1);
        assert_eq!(report.viewers.admitted, 2);
        assert!(report.viewers.finished >= 1);
        assert!(report.source_renewals >= 3);
        assert!(owner.check().is_err());
        drop(peers);
        drop(run);
        assert_eq!(publisher.worker_id(), pid);
        reap(&mut publisher).await;
    }));
}

#[test]
fn desktop_launch_unpolled_cancellation_fences_before_any_callback() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            host,
            h,
            alive,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut agent = super::agent();
        let run = agent
            .run_shared_desktop(
                host,
                &mut publisher,
                &initial,
                service::Policy::default(),
                Duration::from_millis(50),
                entropy(),
                |_, _| panic!("unpolled approval"),
                |_, _| panic!("unpolled event"),
                |_, _| panic!("unpolled publication"),
            )
            .unwrap();
        drop(run);
        assert!(owner.check().is_err());
        assert!(h.checkpoint().is_err());
        assert!(!alive.load(Ordering::Acquire));
        reap(&mut publisher).await;
    });
}

#[test]
fn desktop_launch_parked_time_counts_against_original_host_deadline() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            host,
            h,
            alive,
            viewer: _viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_millis(200)).await;
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut agent = super::agent();
        let mut run = Box::pin(
            agent
                .run_shared_desktop(
                    host,
                    &mut publisher,
                    &initial,
                    service::Policy::default(),
                    Duration::from_millis(50),
                    entropy(),
                    |_, _| panic!("expired approval"),
                    |_, _| panic!("expired event"),
                    |_, _| panic!("expired publication"),
                )
                .unwrap(),
        );
        asupersync::time::sleep(h.timer_driver().unwrap().now(), Duration::from_millis(220)).await;
        assert!(matches!(
            run.as_mut().await,
            Err(DesktopError::Startup(
                crate::session_startup::Error::Expired
            ))
        ));
        assert!(owner.check().is_err());
        assert!(h.checkpoint().is_err());
        assert!(!alive.load(Ordering::Acquire));
        drop(run);
        reap(&mut publisher).await;
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn desktop_launch_publication_faults_fence_reentrant_pending_admission_before_return() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        for mode in 0..3 {
            let first = fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
            let second = fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2)).await;
            let (mut publisher, owner, initial) = selected_publisher(&rt).await;
            let Fresh {
                host,
                c,
                viewer,
                h,
                alive,
            } = *first;
            let Fresh {
                host: second_host,
                h: h2,
                alive: alive2,
                viewer: _second_viewer,
                ..
            } = *second;
            let mut agent = super::agent();
            let cohort = Arc::new(Mutex::new(None));
            let saved = cohort.clone();
            let control = owner.clone();
            let mut run = Box::pin(
                agent
                    .run_shared_desktop(
                        host,
                        &mut publisher,
                        &initial,
                        service::Policy::default(),
                        Duration::from_millis(50),
                        entropy(),
                        |_, _| panic!("unattended first viewer"),
                        |_, _| Ok(LocalAction::Continue),
                        move |admission, first| {
                            let pending = admission
                                .admit_host(second_host, |_, _| {
                                    panic!("pending prompt must be fenced")
                                })
                                .unwrap();
                            assert_eq!(pending.state(), State::Opening);
                            *saved.lock().unwrap() = Some((first, pending));
                            match mode {
                                0 => Err(()),
                                1 => panic!("application publication fault"),
                                _ => {
                                    control.revoke();
                                    Ok(())
                                }
                            }
                        },
                    )
                    .unwrap(),
            );
            let mut client = Box::pin(Client::start(c, viewer));
            let mut retained_client = None;
            let result = poll_fn(|task| {
                let result = catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(task)));
                match result {
                    Err(_) => return Poll::Ready(None),
                    Ok(Poll::Ready(result)) => return Poll::Ready(Some(result)),
                    Ok(Poll::Pending) => {}
                }
                if retained_client.is_none()
                    && let Poll::Ready(client) = client.as_mut().poll(task)
                {
                    retained_client = Some(client);
                }
                Poll::Pending
            })
            .await;
            if mode == 1 {
                assert!(result.is_none());
            } else {
                assert!(result.unwrap().is_err());
            }
            let (first, pending) = cohort.lock().unwrap().take().unwrap();
            assert!(matches!(first.state(), State::Finished(Err(_))));
            assert!(matches!(pending.state(), State::Finished(Err(_))));
            assert!(owner.check().is_err());
            assert!(h.checkpoint().is_err());
            assert!(h2.checkpoint().is_err());
            assert!(!alive.load(Ordering::Acquire));
            assert!(!alive2.load(Ordering::Acquire));
            // The terminal future is deliberately still retained here.
            assert!(matches!(run.as_mut().await, Err(DesktopError::Closed)));
            drop(client);
            drop(retained_client);
            drop(run);
            reap(&mut publisher).await;
        }
    }));
}
