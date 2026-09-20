//! Run incoming negotiation/approval inside the canonical local-consent/capture
//! service, not an independently pre-negotiated helper or a fake network owner.
use super::*;
use crate::session_agent::{
    ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
    source::desktop::LocalAction,
};
use fr_core::input::{DesktopPoint, InputBounds};
fn agent() -> SessionAgent {
    let mut local = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        12,
        InputBounds::new(DesktopPoint { x: -320, y: 40 }, 320, 240).unwrap(),
    );
    local
        .permissions_mut()
        .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    local
}
#[test]
fn incoming_host_managed_service_renews_healthy_viewer_while_local_approval_waits() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let original_worker = publisher.worker_id();
        let mut local_agent = agent();
        let stop = Arc::new(AtomicBool::new(false));
        let local_stop = stop.clone();
        let mut run = Box::pin(
            local_agent
                .serve_shared_desktop(
                    &mut publisher,
                    &mut hub,
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
        let mut client = Box::pin(with_client(&mut first.peer, async {
            let Fresh {
                c,
                host,
                mut viewer,
                h,
                ..
            } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(4))).await;
            let local = Arc::new(Mutex::new(None));
            let copy = local.clone();
            let ticket = admission
                .admit_host(host, move |a, _| {
                    *copy.lock().unwrap() = Some(a);
                    Ok(())
                })
                .unwrap();
            prompt(&mut viewer, &local).await;
            // Existing authority lasts three seconds. The pending newcomer must
            // neither receive observation nor prevent that peer's real renewal.
            let until = now(&h).unwrap() + 3_200_000;
            while now(&h).unwrap() < until {
                viewer.drive(Duration::from_millis(1)).await.unwrap();
            }
            assert!(!viewer.is_complete());
            assert_eq!(ticket.state(), State::Opening);
            assert_eq!(initial.state(), State::Serving);
            assert!(owner.check().is_ok());
            local.lock().unwrap().take().unwrap().decide(true).unwrap();
            let mut second = Box::pin(Client::start(c, viewer)).await;
            second.ready().await;
            assert_eq!(ticket.state(), State::Serving);
            assert_eq!(initial.state(), State::Serving);
            assert_ne!(second.frames, [] as [u64; 0]);
            stop.store(true, Ordering::Release);
            // Keep the newly admitted client alive until the LOCAL stop runs.
            std::future::pending::<()>().await;
        }));
        let report = poll_fn(|task| {
            if let Poll::Ready(report) = run.as_mut().poll(task) {
                return Poll::Ready(report);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await
        .unwrap();
        assert!(stop.load(Ordering::Acquire));
        assert!(report.source_renewals >= 3);
        assert_eq!(report.viewers.admitted, 2);
        assert!(owner.check().is_err());
        drop(client);
        assert!(first.peer.control.check().is_err());
        drop(run);
        assert_eq!(publisher.worker_id(), original_worker);
        reap(&mut publisher).await;
    });
}
#[test]
fn incoming_host_managed_local_permission_loss_fences_unpolled_negotiation_before_notification() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            h,
            alive,
            viewer: _viewer,
            ..
        } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
        let ticket = admission
            .admit_host(host, |_, _| {
                panic!("local loss precedes network/notification")
            })
            .unwrap();
        let mut local = agent();
        let control = owner.clone();
        let mut run = Box::pin(
            local
                .serve_shared_desktop(
                    &mut publisher,
                    &mut hub,
                    Duration::from_millis(50),
                    entropy(),
                    move |agent, _| {
                        let (_, releases) =
                            agent.on_screen_capture_revoked(control.check().unwrap());
                        assert!(releases.is_empty(), "observation never granted input");
                        Ok(LocalAction::Continue)
                    },
                )
                .unwrap(),
        );
        let result = run.as_mut().await;
        assert!(result.is_err());
        assert!(h.checkpoint().is_err());
        assert!(!alive.load(Ordering::Acquire));
        assert!(matches!(ticket.state(), State::Finished(Err(_))));
        assert!(matches!(initial.state(), State::Finished(Err(_))));
        assert!(owner.check().is_err());
        drop(run);
        reap(&mut publisher).await;
    });
}
