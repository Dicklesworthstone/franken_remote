//! Combined production service; client/identity/codec/OS permissions are fixtures.
use super::*;
use crate::session_agent::{
    ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
    source::desktop::{Error as DesktopError, LocalAction, Report},
};
use fr_core::input::{DesktopPoint, InputBounds};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Mutex,
};

fn agent() -> SessionAgent {
    let mut agent = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        12,
        InputBounds::new(DesktopPoint { x: -320, y: 40 }, 320, 240).unwrap(),
    );
    agent
        .permissions_mut()
        .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    agent
}
async fn managed_until<T>(
    run: impl Future<Output = Result<Report, DesktopError>> + Send,
    work: impl Future<Output = T>,
) -> T {
    let mut run = Box::pin(run);
    is_send(&run);
    let mut work = Box::pin(work);
    poll_fn(|task| {
        let result = run.as_mut().poll(task);
        assert!(result.is_pending(), "managed service ended: {result:?}");
        work.as_mut().poll(task)
    })
    .await
}
#[test]
fn desktop_service_renews_source_and_viewer_separately_without_an_external_capture_task() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut peer, mut hub, _admission, ticket) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let original_pid = publisher.worker_id();
        let original_frame = *peer.peer.frames.last().unwrap();
        let mut agent = agent();
        let now0 = owner.check().unwrap().as_micros();
        let until = now0 + 5_200_000;
        let clock = owner.context();
        let run = agent
            .serve_shared_desktop(
                &mut publisher,
                &mut hub,
                Duration::from_millis(50),
                entropy(),
                move |_, _| {
                    Ok(if now(&clock).unwrap() >= until {
                        LocalAction::Stop
                    } else {
                        LocalAction::Continue
                    })
                },
            )
            .unwrap();
        let mut run = Box::pin(run);
        let mut client = Box::pin(async {
            while now(&peer.peer.c).unwrap() < until {
                client_turn(&mut peer.peer).await;
            }
            std::future::pending::<()>().await;
        });
        let result = poll_fn(|cx| {
            if let Poll::Ready(result) = run.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(cx).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        // Terminal result fences authority even with the finished future retained.
        assert!(owner.check().is_err());
        assert!(matches!(ticket.state(), State::Finished(Err(_))));
        assert!(result.source_renewals >= 6);
        drop(client);
        assert!(peer.peer.control.check().is_err());
        assert!(
            peer.peer
                .frames
                .iter()
                .all(|frame| *frame == original_frame)
        );
        drop(run);
        assert_eq!(publisher.worker_id(), original_pid);
        reap(&mut publisher).await;
    });
}

#[test]
fn desktop_service_admits_late_viewers_while_the_first_viewer_keeps_receiving() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, _) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let pid = publisher.worker_id();
        let mut agent = agent();
        let run = agent
            .serve_shared_desktop(
                &mut publisher,
                &mut hub,
                Duration::from_millis(50),
                entropy(),
                |_, _| Ok(LocalAction::Continue),
            )
            .unwrap();
        Box::pin(managed_until(
            run,
            with_client(&mut first.peer, async {
                let (c, h, host, viewer) =
                    sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
                let selection = host.selection().clone();
                let control = host.original_observation();
                let ticket = admission.admit(host).unwrap();
                let mut peer = Box::pin(accept(viewer, c, h, control, selection)).await;
                until_ready(&mut peer.peer).await;
                assert_eq!(ticket.state(), State::Serving);
                assert!(owner.check().is_ok());
                assert!(!peer.peer.control.view_ready().unwrap());
            }),
        ))
        .await;
        assert!(owner.check().is_err());
        assert_eq!(publisher.worker_id(), pid);
        reap(&mut publisher).await;
    });
}

#[test]
fn desktop_unpolled_drop_fences_source_and_pending_admissions_before_cleanup() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, peer, mut hub, admission, initial) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let (_, _, host, _viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let control = host.original_observation();
        let pending = admission.admit(host).unwrap();
        let mut agent = agent();
        let run = agent
            .serve_shared_desktop(
                &mut publisher,
                &mut hub,
                Duration::from_millis(50),
                Arc::new(|| panic!("unpolled entropy")),
                |_, _| panic!("unpolled OS callback"),
            )
            .unwrap();
        is_send(&run);
        drop(run);
        assert!(owner.check().is_err());
        assert!(peer.peer.control.check().is_err());
        assert!(control.check().is_err());
        for ticket in [initial, pending] {
            assert!(matches!(ticket.state(), State::Finished(Err(_))));
        }
        reap(&mut publisher).await;
    });
}

#[test]
fn desktop_local_permission_event_precedes_any_entropy_network_or_capture_turn() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, peer, mut hub, _, initial) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let mut agent = agent();
        let mut run = Box::pin(
            agent
                .serve_shared_desktop(
                    &mut publisher,
                    &mut hub,
                    Duration::from_millis(50),
                    Arc::new(|| panic!("permission refusal precedes entropy")),
                    |agent, _| {
                        agent.permissions_mut().set_permission(
                            PermissionKind::ScreenCapture,
                            PermissionStatus::Denied,
                        );
                        Ok(LocalAction::Continue)
                    },
                )
                .unwrap(),
        );
        let error = poll_fn(|cx| run.as_mut().poll(cx)).await.unwrap_err();
        assert!(matches!(
            error,
            DesktopError::Consent(crate::session_agent::source::Error::NoCapturePermission)
        ));
        assert!(owner.check().is_err());
        assert!(peer.peer.control.check().is_err());
        assert!(matches!(initial.state(), State::Finished(Err(_))));
        drop(run);
        reap(&mut publisher).await;
    });
}

#[test]
fn desktop_refuses_foreign_source_without_revoking_either_original_owner() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut first, first_owner, _peer, mut hub, _, ticket) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let (mut foreign, foreign_owner, _initial) = selected_publisher(&rt).await;
        let mut agent = agent();
        let result = agent.serve_shared_desktop(
            &mut foreign,
            &mut hub,
            Duration::from_millis(50),
            entropy(),
            |_, _| panic!("foreign source callback"),
        );
        assert!(matches!(
            result,
            Err(DesktopError::Viewers(service::Error::Source(
                crate::media::shared_publisher::Error::WrongSource
            )))
        ));
        drop(result);
        assert!(first_owner.check().is_ok());
        assert!(foreign_owner.check().is_ok());
        assert_eq!(ticket.state(), State::Serving);
        hub.close();
        reap(&mut first).await;
        reap(&mut foreign).await;
    });
}

#[test]
fn desktop_refuses_another_local_agent_without_stealing_or_revoking_its_source() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _peer, mut hub, _, _) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let mut original = agent();
        original.attach_shared_source(&publisher).unwrap();
        let mut foreign = agent();
        let result = foreign.serve_shared_desktop(
            &mut publisher,
            &mut hub,
            Duration::from_millis(50),
            entropy(),
            |_, _| panic!("foreign local callback"),
        );
        assert!(matches!(result, Err(DesktopError::Consent(_))));
        drop(result);
        assert!(owner.check().is_ok());
        original.service_shared_sources(|| Ok(730_001)).unwrap();
        assert!(owner.check().is_ok());
        hub.close();
        reap(&mut publisher).await;
    });
}

#[test]
fn desktop_entropy_failure_and_local_callback_unwind_fence_even_a_retained_future() {
    let rt = support::runtime();
    rt.block_on(async {
        for panic_in_hook in [false, true] {
            let (mut publisher, owner, peer, mut hub, _, ticket) =
                fixture(&rt, service::Policy::default(), entropy()).await;
            let mut agent = agent();
            let mut run = Box::pin(
                agent
                    .serve_shared_desktop(
                        &mut publisher,
                        &mut hub,
                        Duration::from_millis(50),
                        Arc::new(|| Err(())),
                        move |_, _| {
                            assert!(!panic_in_hook, "intentional local adapter unwind");
                            Ok(LocalAction::Continue)
                        },
                    )
                    .unwrap(),
            );
            poll_fn(|cx| {
                let result = catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(cx)));
                if panic_in_hook {
                    assert!(result.is_err());
                } else {
                    assert!(matches!(
                        result,
                        Ok(Poll::Ready(Err(DesktopError::Consent(_))))
                    ));
                }
                Poll::Ready(())
            })
            .await;
            assert!(owner.check().is_err());
            assert!(peer.peer.control.check().is_err());
            assert!(matches!(ticket.state(), State::Finished(Err(_))));
            drop(run);
            reap(&mut publisher).await;
        }
    });
}

#[test]
fn desktop_pending_capture_does_not_delay_event_revocation_or_create_timer_backlog() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut peer, mut hub, _, ticket) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let pid = publisher.worker_id().unwrap();
        let mut agent = agent();
        let stop = Arc::new(AtomicBool::new(false));
        let waker = Arc::new(Mutex::new(None::<std::task::Waker>));
        let (flag, event_waker) = (stop.clone(), waker.clone());
        let driver = owner.context().timer_driver().unwrap();
        let before = driver.pending_count();
        let mut run = Box::pin(
            agent
                .serve_shared_desktop(
                    &mut publisher,
                    &mut hub,
                    Duration::from_millis(50),
                    entropy(),
                    move |agent, cx| {
                        *event_waker.lock().unwrap() = Some(cx.waker().clone());
                        if flag.load(Ordering::Acquire) {
                            agent.permissions_mut().set_permission(
                                PermissionKind::ScreenCapture,
                                PermissionStatus::Denied,
                            );
                        }
                        Ok(LocalAction::Continue)
                    },
                )
                .unwrap(),
        );
        // Freeze only this test's child. A pending native read may never prevent
        // the trusted local event from fencing observation on the next poll.
        assert!(
            std::process::Command::new("kill")
                .args(["-STOP", &pid.to_string()])
                .status()
                .unwrap()
                .success()
        );
        let mut client = Box::pin(async {
            let until = now(&peer.peer.c).unwrap() + 100_000;
            while now(&peer.peer.c).unwrap() < until {
                client_turn(&mut peer.peer).await;
            }
            stop.store(true, Ordering::Release);
            waker.lock().unwrap().as_ref().unwrap().wake_by_ref();
            std::future::pending::<()>().await;
        });
        let result = poll_fn(|cx| {
            if let Poll::Ready(result) = run.as_mut().poll(cx) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(cx).is_pending());
            Poll::Pending
        })
        .await;
        assert!(matches!(result, Err(DesktopError::Consent(_))));
        assert!(owner.check().is_err());
        assert!(matches!(ticket.state(), State::Finished(Err(_))));
        assert!(
            driver.pending_count() <= before,
            "terminal service releases its timer"
        );
        drop(client);
        drop(run);
        reap(&mut publisher).await;
    });
}

#[test]
fn desktop_local_stop_does_not_revoke_another_source_registered_to_the_same_agent() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _peer, mut hub, _, _) =
            fixture(&rt, service::Policy::default(), entropy()).await;
        let (mut other, other_owner, _initial) = selected_publisher(&rt).await;
        let mut agent = agent();
        agent.attach_shared_source(&other).unwrap();
        let mut run = Box::pin(
            agent
                .serve_shared_desktop(
                    &mut publisher,
                    &mut hub,
                    Duration::from_millis(50),
                    entropy(),
                    |_, _| Ok(LocalAction::Stop),
                )
                .unwrap(),
        );
        let report = poll_fn(|cx| run.as_mut().poll(cx)).await.unwrap();
        assert_eq!(report.source_renewals, 0);
        assert!(owner.check().is_err());
        assert!(other_owner.check().is_ok());
        drop(run);
        assert!(!agent.is_revoked());
        reap(&mut publisher).await;
        reap(&mut other).await;
    });
}
