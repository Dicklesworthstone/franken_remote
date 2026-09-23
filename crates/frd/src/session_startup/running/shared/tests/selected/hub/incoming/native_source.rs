//! Real TLS/UDP and supervised process IPC; OS/codec responses are fixtures.
use super::super::super::super::preparation;
use super::*;
use crate::{
    session_agent::source::{
        desktop::{Error as DesktopError, LocalAction},
        prepare::Setup,
    },
    session_agent::{PermissionKind, PermissionStatus, SessionAgent},
    worker::Retirement,
};
use std::{path::PathBuf, pin::Pin, task::Context};

type Receipt = Arc<Mutex<Option<(ObservationControl, Retirement, PathBuf)>>>;
fn factory<'a>(
    rt: &'a Runtime,
    receipt: Receipt,
    mode: &'a str,
) -> impl FnOnce() -> Result<Setup, ()> + Send + 'a {
    move || {
        let (setup, control, retirement, trace) = preparation::setup(rt, mode);
        *receipt.lock().unwrap() = Some((control, retirement, trace));
        Ok(setup)
    }
}
fn local(_: &mut SessionAgent, _: &mut Context<'_>) -> Result<LocalAction, ()> {
    Ok(LocalAction::Continue)
}
async fn cleanup(receipt: &Receipt, spawned: bool) {
    let record = receipt.lock().unwrap().take();
    if let Some((control, mut retirement, _)) = record {
        assert!(control.check().is_err());
        let cx = Cx::current().unwrap();
        let exited = retirement
            .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
            .await
            .unwrap();
        assert_eq!(exited.is_some(), spawned);
    } else {
        assert!(!spawned);
    }
}
async fn refused(
    mut work: Pin<
        Box<
            impl Future<
                Output = Result<crate::session_agent::source::desktop::NativeDesktop, DesktopError>,
            >,
        >,
    >,
    mut viewer: Viewer,
) -> DesktopError {
    let mut client = Box::pin(async {
        loop {
            let _ = viewer.drive(Duration::from_millis(1)).await;
            asupersync::runtime::yield_now().await;
        }
    });
    poll_fn(|task| {
        if let Poll::Ready(result) = work.as_mut().poll(task) {
            return Poll::Ready(result.unwrap_err());
        }
        assert!(client.as_mut().poll(task).is_pending());
        Poll::Pending
    })
    .await
}

#[test]
fn first_native_source_is_created_only_after_consent_and_streams_on_the_same_child() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            c,
            h,
            host,
            mut viewer,
            ..
        } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(4)).await;
        let receipt = Receipt::default();
        let prompt_slot = Arc::new(Mutex::new(None));
        let notify = prompt_slot.clone();
        let mut agent = preparation::agent();
        let random = entropy();
        let opening = agent
            .open_native_shared_desktop(
                host,
                factory(&rt, receipt.clone(), "normal"),
                preparation::choose,
                service::Policy::default(),
                random.clone(),
                move |a, role| {
                    assert_eq!(role, Role::Observe);
                    *notify.lock().unwrap() = Some(a);
                    Ok(())
                },
                local,
            )
            .unwrap();
        is_send(&opening);
        let (desktop, mut client) = Box::pin(support::both(opening, async {
            prompt(&mut viewer, &prompt_slot).await;
            // Longer than the unused-source lifetime. Nothing native exists yet.
            let until = now(&c).unwrap() + 2_050_000;
            while now(&c).unwrap() < until {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
                assert!(receipt.lock().unwrap().is_none());
            }
            prompt_slot
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .decide(true)
                .unwrap();
            Client::start(c, viewer).await
        }))
        .await;
        let mut desktop = desktop.unwrap();
        let pid = desktop.worker_id().unwrap();
        let ticket = desktop.first();
        let control = receipt.lock().unwrap().as_ref().unwrap().0.clone();
        assert!(!control.view_ready().unwrap());
        assert!(h.checkpoint().is_ok());
        let trace = receipt.lock().unwrap().as_ref().unwrap().2.clone();
        assert_eq!(
            std::fs::read_to_string(trace).unwrap().trim(),
            pid.to_string()
        );
        let stop = Arc::new(AtomicBool::new(false));
        let signal = stop.clone();
        let (publisher, hub) = desktop.parts();
        let mut server = Box::pin(
            agent
                .serve_shared_desktop(
                    publisher,
                    hub,
                    Duration::from_millis(50),
                    random,
                    move |_, _| {
                        Ok(if signal.load(Ordering::Acquire) {
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
            assert!(!client.frames.is_empty());
            stop.store(true, Ordering::Release);
            std::future::pending::<()>().await;
        });
        let report = poll_fn(|task| {
            if let Poll::Ready(result) = server.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(peer.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert!(report.source_renewals >= 3);
        drop(peer);
        drop(server);
        drop(desktop);
        cleanup(&receipt, true).await;
    });
}

#[test]
fn denial_and_control_intent_never_call_the_source_factory() {
    let rt = support::runtime();
    rt.block_on(async {
        for role in [Role::Observe, Role::RequestControl] {
            let Fresh {
                h, host, viewer, ..
            } = *fresh(&rt, 13, true, role, Duration::from_secs(2)).await;
            let receipt = Receipt::default();
            let mut agent = preparation::agent();
            let run = agent
                .open_native_shared_desktop(
                    host,
                    factory(&rt, receipt.clone(), "normal"),
                    preparation::choose,
                    service::Policy::default(),
                    entropy(),
                    move |a, _| {
                        assert_eq!(role, Role::Observe);
                        a.decide(false).unwrap();
                        Ok(())
                    },
                    local,
                )
                .unwrap();
            let error = refused(Box::pin(run), viewer).await;
            assert!(matches!(error, DesktopError::Startup(_)));
            assert!(h.checkpoint().is_err());
            assert!(receipt.lock().unwrap().is_none());
        }
    });
}
#[test]
fn unpolled_and_expired_first_attempts_never_create_a_source() {
    let rt = support::runtime();
    rt.block_on(async {
        for expired in [false, true] {
            let Fresh {
                h, host, viewer, ..
            } = *fresh(&rt, 13, true, Role::Observe, Duration::from_millis(100)).await;
            let receipt = Receipt::default();
            let mut agent = preparation::agent();
            let run = agent
                .open_native_shared_desktop(
                    host,
                    factory(&rt, receipt.clone(), "normal"),
                    preparation::choose,
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("no prompt"),
                    local,
                )
                .unwrap();
            if expired {
                asupersync::time::sleep(Cx::current().unwrap().now(), Duration::from_millis(120))
                    .await;
                assert!(matches!(
                    refused(Box::pin(run), viewer).await,
                    DesktopError::Startup(crate::session_startup::Error::Expired)
                ));
            } else {
                drop(run);
                drop(viewer);
            }
            assert!(h.checkpoint().is_err());
            assert!(receipt.lock().unwrap().is_none());
        }
    });
}
#[test]
fn initial_permission_and_os_session_refusals_never_enter_network_or_native_callbacks() {
    let rt = support::runtime();
    rt.block_on(async {
        for locked in [false, true] {
            let Fresh {
                h,
                host,
                viewer: _viewer,
                ..
            } = *fresh(&rt, 13, true, Role::Observe, Duration::from_secs(2)).await;
            let mut agent = preparation::agent();
            if locked {
                let _ = agent.on_os_session_changed(99, fr_core::time::HostInstant::from_micros(0));
            } else {
                agent
                    .permissions_mut()
                    .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Denied);
            }
            let result = agent.open_native_shared_desktop(
                host,
                || panic!("no factory"),
                preparation::choose,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("no prompt"),
                local,
            );
            assert!(matches!(result, Err(DesktopError::Consent(_))));
            assert!(h.checkpoint().is_err());
        }
    });
}
#[test]
fn factory_failure_and_local_stop_preserve_the_cause_without_spawning() {
    let rt = support::runtime();
    rt.block_on(async {
        for stop in [false, true] {
            let Fresh {
                h, host, viewer, ..
            } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
            let mut agent = preparation::agent();
            let called = Arc::new(AtomicBool::new(false));
            let count = called.clone();
            let run = agent
                .open_native_shared_desktop(
                    host,
                    move || {
                        count.store(true, Ordering::Release);
                        Err(())
                    },
                    preparation::choose,
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("unattended"),
                    move |_, _| {
                        Ok(if stop {
                            LocalAction::Stop
                        } else {
                            LocalAction::Continue
                        })
                    },
                )
                .unwrap();
            let error = refused(Box::pin(run), viewer).await;
            assert_eq!(
                error,
                if stop {
                    DesktopError::Closed
                } else {
                    DesktopError::SourceSetup
                }
            );
            assert_eq!(called.load(Ordering::Acquire), !stop);
            assert!(h.checkpoint().is_err());
        }
    });
}
#[test]
fn permission_loss_during_stalled_capture_fences_both_owners_before_reaping() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            h, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
        let receipt = Receipt::default();
        let events = receipt.clone();
        let mut agent = preparation::agent();
        let run = agent
            .open_native_shared_desktop(
                host,
                factory(&rt, receipt.clone(), "stall-capture"),
                preparation::choose,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("unattended"),
                move |agent, _| {
                    if events.lock().unwrap().as_ref().is_some_and(|(_, _, p)| {
                        std::fs::read_to_string(p).is_ok_and(|s| s.contains("capture"))
                    }) {
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
        assert!(matches!(error, DesktopError::Preparation(_)), "{error:?}");
        assert!(h.checkpoint().is_err());
        cleanup(&receipt, true).await;
    });
}
#[test]
fn selector_panic_fences_native_and_peer_even_when_failed_future_is_retained() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            h,
            host,
            mut viewer,
            ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
        let receipt = Receipt::default();
        let mut agent = preparation::agent();
        let mut run = Box::pin(
            agent
                .open_native_shared_desktop(
                    host,
                    factory(&rt, receipt.clone(), "normal"),
                    |_| panic!("selector fixture"),
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("unattended"),
                    local,
                )
                .unwrap(),
        );
        let mut client = Box::pin(async {
            loop {
                let _ = viewer.drive(Duration::from_millis(1)).await;
            }
        });
        poll_fn(|task| {
            if catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(task))).is_err() {
                return Poll::Ready(());
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        assert!(h.checkpoint().is_err());
        assert!(receipt.lock().unwrap().as_ref().unwrap().0.check().is_err());
        drop(client);
        drop(run);
        cleanup(&receipt, true).await;
    });
}

#[test]
fn a_foreign_registered_source_is_not_revoked_on_first_source_refusal() {
    let rt = support::runtime();
    rt.block_on(async {
        let (original, control, mut retirement, _) = preparation::setup(&rt, "normal");
        let mut original_agent = preparation::agent();
        let prepared = original_agent
            .prepare_native_shared_source(original, preparation::choose, local)
            .unwrap()
            .await
            .unwrap();
        let pid = prepared.worker_id();
        let (mut replacement, unused, mut unstarted, _) = preparation::setup(&rt, "normal");
        unused.revoke();
        replacement.control = control.clone();
        let Fresh {
            h, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_secs(2)).await;
        let mut foreign = preparation::agent();
        let opening = foreign
            .open_native_shared_desktop(
                host,
                || Ok(replacement),
                preparation::choose,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("unattended"),
                local,
            )
            .unwrap();
        assert!(matches!(
            refused(Box::pin(opening), viewer).await,
            DesktopError::Preparation(_)
        ));
        assert!(h.checkpoint().is_err());
        assert!(
            control.check().is_ok(),
            "refused source still belongs to original agent"
        );
        assert_eq!(prepared.worker_id(), pid);
        let cx = Cx::current().unwrap();
        assert!(
            unstarted
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap()
                .is_none()
        );
        drop(prepared);
        assert!(
            retirement
                .reap(&cx, Deadline::after(&cx, Duration::from_secs(1)).unwrap())
                .await
                .unwrap()
                .is_some()
        );
    });
}
#[test]
fn factory_time_cannot_restart_the_first_host_deadline() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            h, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_millis(150)).await;
        let until = host.deadline_us();
        let receipt = Receipt::default();
        let record = receipt.clone();
        let mut agent = preparation::agent();
        let mut run = Box::pin(
            agent
                .open_native_shared_desktop(
                    host,
                    || {
                        let source = factory(&rt, record, "normal")()?;
                        // Deliberately blocking local fixture: check its elapsed work rather
                        // than pretending this callback can be preempted by an async timer.
                        while now(&h).unwrap() <= until {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Ok(source)
                    },
                    |_| panic!("expired before native selection"),
                    service::Policy::default(),
                    entropy(),
                    |_, _| panic!("unattended"),
                    local,
                )
                .unwrap(),
        );
        assert!(matches!(
            refused(Box::pin(run.as_mut()), viewer).await,
            DesktopError::Startup(crate::session_startup::Error::Expired)
        ));
        drop(run);
        assert!(h.checkpoint().is_err());
        assert!(!receipt.lock().unwrap().as_ref().unwrap().2.exists());
        cleanup(&receipt, false).await;
    });
}
#[test]
fn original_host_expiry_ends_stalled_native_work_before_the_native_budget() {
    let rt = support::runtime();
    rt.block_on(async {
        let Fresh {
            h, host, viewer, ..
        } = *fresh(&rt, 13, false, Role::Observe, Duration::from_millis(250)).await;
        let receipt = Receipt::default();
        let mut agent = preparation::agent();
        let start = std::time::Instant::now();
        let run = agent
            .open_native_shared_desktop(
                host,
                factory(&rt, receipt.clone(), "stall-capture"),
                preparation::choose,
                service::Policy::default(),
                entropy(),
                |_, _| panic!("unattended"),
                local,
            )
            .unwrap();
        assert!(matches!(
            refused(Box::pin(run), viewer).await,
            DesktopError::Startup(crate::session_startup::Error::Expired)
        ));
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "do not wait for native two-second budget"
        );
        assert!(h.checkpoint().is_err());
        cleanup(&receipt, true).await;
    });
}

#[path = "native_run.rs"]
mod run;

#[path = "async_source.rs"]
mod async_source;
