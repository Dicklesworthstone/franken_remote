//! Actual TLS/UDP/process service; synthetic OS/codec facts are explicit fixtures.
use super::*;
use crate::session_agent::source::desktop::Report;

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
async fn drive_report(
    run: &mut (impl Future<Output = Result<Report, DesktopError>> + Unpin),
    peers: impl Future<Output = ()>,
) -> Result<Report, DesktopError> {
    let mut peers = Box::pin(peers);
    poll_fn(|task| {
        if let Poll::Ready(result) = Pin::new(&mut *run).poll(task) {
            return Poll::Ready(result);
        }
        assert!(peers.as_mut().poll(task).is_pending());
        Poll::Pending
    })
    .await
}

#[test]
fn native_run_admits_a_late_viewer_and_keeps_the_same_child_after_first_departure() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        let first = fresh(&rt, 13, false, Role::Observe, Duration::from_secs(3)).await;
        let second = fresh(&rt, 14, false, Role::Observe, Duration::from_secs(4)).await;
        let Fresh {
            c, h, host, viewer, ..
        } = *first;
        let Fresh {
            c: c2,
            host: host2,
            viewer: viewer2,
            ..
        } = *second;
        let receipt = Receipt::default();
        let mut agent = preparation::agent();
        let advertised = Arc::new(Mutex::new(None));
        let saved = advertised.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let calls = Arc::new(AtomicU64::new(0));
        let called = calls.clone();
        let events = Arc::new(AtomicU64::new(0));
        let observed = events.clone();
        let mut local_count = 0;
        let mut run = Box::pin(
            agent
                .run_native_shared_desktop(
                    host,
                    factory(&rt, receipt.clone(), "changing"),
                    preparation::choose,
                    service::Policy::default(),
                    Duration::from_millis(50),
                    entropy(),
                    |_, _| panic!("unattended"),
                    move |_, _| {
                        local_count += 1;
                        assert_eq!(observed.fetch_add(1, Ordering::Relaxed) + 1, local_count);
                        Ok(if signal.load(Ordering::Acquire) {
                            LocalAction::Stop
                        } else {
                            LocalAction::Continue
                        })
                    },
                    move |admission, ticket| {
                        assert_eq!(called.fetch_add(1, Ordering::Relaxed), 0);
                        assert_eq!(ticket.state(), State::Serving);
                        *saved.lock().unwrap() = Some((admission, ticket));
                        Ok(())
                    },
                )
                .unwrap(),
        );
        is_send(&run);
        let peers = async {
            let mut first = Client::start(c, viewer).await;
            first.ready().await;
            let (admission, ticket) = advertised.lock().unwrap().take().unwrap();
            let (control, trace) = {
                let receipt = receipt.lock().unwrap();
                let (control, _, trace) = receipt.as_ref().unwrap();
                (control.clone(), trace.clone())
            };
            let child = std::fs::read_to_string(&trace).unwrap();
            let startup_events = events.load(Ordering::Relaxed);
            assert!(startup_events > 0);
            assert!(!control.view_ready().unwrap());
            let late = admission
                .admit_host(host2, |_, _| panic!("unattended late peer"))
                .unwrap();
            let mut second = consume(&mut first, async {
                let mut second = Client::start(c2.clone(), viewer2).await;
                second.ready().await;
                second
            })
            .await;
            ticket.close();
            first.viewer.close();
            let until = now(&c2).unwrap() + 3_100_000;
            while now(&c2).unwrap() < until {
                second.turn().await;
            }
            assert!(h.checkpoint().is_err());
            assert!(matches!(ticket.state(), State::Finished(Err(_))));
            assert_eq!(late.state(), State::Serving);
            assert!(second.frames.len() > 1);
            assert!(control.check().is_ok());
            assert_eq!(std::fs::read_to_string(trace).unwrap(), child);
            assert!(events.load(Ordering::Relaxed) > startup_events);
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        };
        let report = drive_report(&mut run, peers).await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(report.viewers.admitted, 2);
        assert!(report.viewers.finished >= 1);
        assert!(report.source_renewals >= 3);
        // Observe cleanup while the completed outer service is still retained.
        cleanup(&receipt, true).await;
        assert!(matches!(run.as_mut().await, Err(DesktopError::Closed)));
    }));
}

#[test]
fn native_run_unpolled_and_expired_attempts_never_call_source_or_publication_callbacks() {
    let rt = support::runtime();
    rt.block_on(async {
        for expired in [false, true] {
            let Fresh {
                h,
                host,
                viewer: _viewer,
                ..
            } = *fresh(&rt, 13, true, Role::Observe, Duration::from_millis(100)).await;
            let receipt = Receipt::default();
            let mut agent = preparation::agent();
            let mut run = Box::pin(
                agent
                    .run_native_shared_desktop(
                        host,
                        factory(&rt, receipt.clone(), "normal"),
                        preparation::choose,
                        service::Policy::default(),
                        Duration::from_millis(50),
                        entropy(),
                        |_, _| panic!("unpolled approval"),
                        |_, _| panic!("unpolled local events"),
                        |_, _| panic!("unpolled publication"),
                    )
                    .unwrap(),
            );
            if expired {
                asupersync::time::sleep(h.now(), Duration::from_millis(120)).await;
                assert!(matches!(
                    run.as_mut().await,
                    Err(DesktopError::Startup(OpenError::Expired))
                ));
            }
            drop(run);
            assert!(receipt.lock().unwrap().is_none());
            assert!(h.checkpoint().is_err());
        }
    });
}

#[test]
fn invalid_native_service_cadence_refuses_before_network_or_source_creation() {
    let rt = support::runtime();
    rt.block_on(async {
        for cadence in [Duration::ZERO, Duration::from_secs(2)] {
            let Fresh {
                h,
                host,
                viewer: _viewer,
                ..
            } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
            let mut agent = preparation::agent();
            let result = agent.run_native_shared_desktop(
                host,
                || panic!("invalid cadence cannot launch"),
                preparation::choose,
                service::Policy::default(),
                cadence,
                entropy(),
                |_, _| panic!("approval"),
                local,
                |_, _| panic!("announce"),
            );
            assert!(matches!(
                result,
                Err(DesktopError::Capture(
                    crate::media::shared_publisher::Error::InvalidBudget
                ))
            ));
            assert!(h.checkpoint().is_err());
        }
    });
}

#[test]
fn native_run_publication_faults_fence_source_and_reentrant_join_before_future_drop() {
    let rt = support::runtime();
    rt.block_on(Box::pin(async {
        for mode in 0..3 {
            let first = fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
            let second = fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2)).await;
            let Fresh {
                c, h, host, viewer, ..
            } = *first;
            let Fresh {
                h: h2,
                host: second_host,
                viewer: _second,
                ..
            } = *second;
            let receipt = Receipt::default();
            let owner = receipt.clone();
            let mut agent = preparation::agent();
            let cohort = Arc::new(Mutex::new(None));
            let saved = cohort.clone();
            let mut run = Box::pin(
                agent
                    .run_native_shared_desktop(
                        host,
                        factory(&rt, receipt.clone(), "normal"),
                        preparation::choose,
                        service::Policy::default(),
                        Duration::from_millis(50),
                        entropy(),
                        |_, _| panic!("unattended"),
                        local,
                        move |admission, first| {
                            let pending = admission
                                .admit_host(second_host, |_, _| panic!("fenced pending join"))
                                .unwrap();
                            *saved.lock().unwrap() = Some((first, pending));
                            match mode {
                                0 => Err(()),
                                1 => panic!("local announcement failure"),
                                _ => {
                                    owner.lock().unwrap().as_ref().unwrap().0.revoke();
                                    Ok(())
                                }
                            }
                        },
                    )
                    .unwrap(),
            );
            let mut client = Box::pin(Client::start(c, viewer));
            let mut retained = None;
            let outcome = poll_fn(|task| {
                match catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(task))) {
                    Err(_) => return Poll::Ready(None),
                    Ok(Poll::Ready(result)) => return Poll::Ready(Some(result)),
                    Ok(Poll::Pending) => {}
                }
                if retained.is_none()
                    && let Poll::Ready(client) = client.as_mut().poll(task)
                {
                    retained = Some(client);
                }
                Poll::Pending
            })
            .await;
            match mode {
                0 => assert_eq!(outcome.unwrap(), Err(DesktopError::LocalEvent)),
                1 => assert!(outcome.is_none()),
                _ => assert!(outcome.unwrap().is_err()),
            }
            let (first, pending) = cohort.lock().unwrap().take().unwrap();
            for ticket in [first, pending] {
                assert!(matches!(ticket.state(), State::Finished(Err(_))));
            }
            assert!(h.checkpoint().is_err());
            assert!(h2.checkpoint().is_err());
            // Not just authority revocation: observe actual original child exit.
            cleanup(&receipt, true).await;
            assert!(matches!(run.as_mut().await, Err(DesktopError::Closed)));
            drop(client);
            drop(retained);
            drop(run);
        }
    }));
}

#[test]
fn native_run_local_permission_loss_after_streaming_stops_without_a_caller_handoff() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            c, h, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
        let receipt = Receipt::default();
        let revoke = Arc::new(AtomicBool::new(false));
        let signal = revoke.clone();
        let mut agent = preparation::agent();
        let mut run = Box::pin(
            agent
                .run_native_shared_desktop(
                    host,
                    factory(&rt, receipt.clone(), "normal"),
                    preparation::choose,
                    service::Policy::default(),
                    Duration::from_millis(50),
                    entropy(),
                    |_, _| panic!("unattended"),
                    move |agent, _| {
                        if signal.load(Ordering::Acquire) {
                            agent.permissions_mut().set_permission(
                                PermissionKind::ScreenCapture,
                                PermissionStatus::Denied,
                            );
                        }
                        Ok(LocalAction::Continue)
                    },
                    |_, _| Ok(()),
                )
                .unwrap(),
        );
        let peers = async {
            let mut client = Client::start(c, viewer).await;
            client.ready().await;
            assert!(!client.frames.is_empty());
            revoke.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        };
        assert!(matches!(
            drive_report(&mut run, peers).await,
            Err(DesktopError::Consent(_))
        ));
        assert!(h.checkpoint().is_err());
        cleanup(&receipt, true).await;
    });
}
