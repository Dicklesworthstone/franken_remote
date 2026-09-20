//! Local consent protects the original native source before any viewer exists.
//! Real source IPC and TLS/UDP; permission and HEVC content remain explicit fixtures.
use super::*;
use crate::{
    input_watchdog::StopReason,
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{Error as ConsentError, Status},
    },
};
use fr_core::{
    input::{DesktopPoint, InputBounds},
    time::HostInstant,
};

fn agent(granted: bool) -> SessionAgent {
    let mut agent = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        12,
        InputBounds::new(DesktopPoint { x: -320, y: 40 }, 320, 240).unwrap(),
    );
    if granted {
        agent
            .permissions_mut()
            .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    }
    agent
}
fn service(agent: &mut SessionAgent, value: u128) -> Result<Status, ConsentError> {
    let report = agent.service_shared_sources(|| Ok(value)).unwrap();
    assert_eq!(report.outcomes().count(), 1);
    *report.outcomes().next().unwrap()
}

#[test]
fn local_consent_attaches_before_first_viewer_and_survives_original_session_bootstrap() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        assert_eq!(publisher.tick().unwrap(), 0);
        let original_worker = publisher.worker_id();
        let mut local = agent(true);
        local.attach_shared_source(&publisher).unwrap();
        let before = service(&mut local, 51001).unwrap();
        assert!(before.renewed);
        assert!(!owner.view_ready().unwrap());
        let peer = Box::pin(first(&rt, &mut publisher, &initial, 13)).await;
        assert_eq!(publisher.worker_id(), original_worker);
        assert!(peer.peer.control.check().is_ok());
        assert!(!peer.peer.control.view_ready().unwrap());
        let after = service(&mut local, 51002).unwrap();
        assert_eq!(after.authorized_until, before.authorized_until);
        assert!(
            !after.renewed,
            "first viewer does not reset consent cadence"
        );
        drop(local);
        assert!(owner.check().is_err());
        assert!(peer.peer.control.check().is_err());
        close(&mut publisher, vec![peer]).await;
    });
}

#[test]
fn pre_viewer_revoke_lock_session_loss_and_drop_fence_without_another_poll() {
    let rt = support::runtime();
    rt.block_on(async {
        for reason in 0..5 {
            let (mut publisher, owner, initial) = selected_publisher(&rt).await;
            let queue = publisher.join_queue();
            let mut local = agent(true);
            local.attach_shared_source(&publisher).unwrap();
            let instant = owner.check().unwrap();
            match reason {
                0 => {
                    local
                        .indicator()
                        .immediate_revoke(instant, StopReason::AuthorityEnded);
                }
                1 => local.on_os_locked(instant),
                2 => local.on_os_session_changed(13, instant),
                3 => {
                    let (_, releases) = local.on_screen_capture_revoked(instant);
                    assert!(releases.is_empty(), "no input authority was ever granted");
                }
                _ => {
                    drop(local);
                    assert!(owner.check().is_err());
                    assert!(queue.selected_catalog().is_err());
                    drop(initial);
                    close(&mut publisher, vec![]).await;
                    continue;
                }
            }
            assert!(owner.check().is_err());
            assert!(queue.selected_catalog().is_err());
            local
                .permissions_mut()
                .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
            assert!(local.attach_shared_source(&publisher).is_err());
            drop(initial);
            close(&mut publisher, vec![]).await;
        }
    });
}

#[test]
fn consent_renewal_never_extends_the_unused_publishers_bootstrap_budget() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let opening = owner.check().unwrap();
        let mut local = agent(true);
        local.attach_shared_source(&publisher).unwrap();
        let first = service(&mut local, 61001).unwrap();
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            first.next_check.as_micros() * 1000,
        ))
        .await;
        let second = service(&mut local, 61002).unwrap();
        assert!(second.renewed);
        assert!(second.authorized_until > first.authorized_until);
        assert!(second.next_check.as_micros() <= opening.as_micros() + 2_000_000);
        assert!(second.next_check < second.authorized_until);
        assert_eq!(publisher.tick().unwrap(), 0);
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
            second.next_check.as_micros() * 1000,
        ))
        .await;
        assert!(matches!(
            service(&mut local, 61003),
            Err(ConsentError::Publisher(_))
        ));
        assert!(owner.check().is_err());
        assert!(publisher.join_queue().selected_catalog().is_err());
        drop(initial);
        close(&mut publisher, vec![]).await;
    });
}

#[test]
fn pre_viewer_permission_and_duplicate_checks_preserve_the_original_owner() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut missing = agent(false);
        assert_eq!(
            missing.attach_shared_source(&publisher),
            Err(ConsentError::NoCapturePermission)
        );
        assert!(owner.check().is_ok());
        let mut local = agent(true);
        local.attach_shared_source(&publisher).unwrap();
        assert_eq!(
            local.attach_shared_source(&publisher),
            Err(ConsentError::AlreadyAttached)
        );
        let mut other = agent(true);
        assert_eq!(
            other.attach_shared_source(&publisher),
            Err(ConsentError::AlreadyAttached)
        );
        drop((missing, other));
        let before = service(&mut local, 71001).unwrap();
        let report = local
            .service_shared_sources(|| panic!("no cadence reset on duplicate attach"))
            .unwrap();
        assert_eq!(report.next_deadline(), Some(before.next_check));
        assert!(!report.outcomes().next().unwrap().as_ref().unwrap().renewed);
        local
            .permissions_mut()
            .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Denied);
        assert_eq!(
            service(&mut local, 71002),
            Err(ConsentError::NoCapturePermission)
        );
        assert!(owner.check().is_err());
        drop(initial);
        close(&mut publisher, vec![]).await;
    });
}

#[test]
fn a_nonselected_source_still_requires_its_original_admitted_scope() {
    let rt = support::runtime();
    rt.block_on(async {
        let mut g = Box::pin(group(&rt, 0)).await;
        let mut local = agent(true);
        assert_eq!(
            local.attach_shared_source(&g.publisher),
            Err(ConsentError::WrongScope)
        );
        assert!(g.owner.check().is_ok());
        assert_eq!(g.publisher.tick().unwrap(), 0);
        let cx = Cx::current().unwrap();
        cleanup(&mut g, &cx).await;
    });
}

#[test]
fn first_viewer_ticket_generation_cannot_pass_a_local_indicator_revoke() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut local = agent(true);
        local.attach_shared_source(&publisher).unwrap();
        let (c, h, host, mut viewer) =
            sessions(&rt, 13, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let selection = host.selection().clone();
        let control = host.original_observation();
        let chosen = AtomicBool::new(false);
        let revoked = AtomicBool::new(false);
        let mut n = 81000;
        let opening = host.start_shared_display(
            &mut publisher,
            &initial,
            Duration::from_secs(2),
            SendPolicy::default(),
            || {
                if chosen.load(Ordering::Acquire) {
                    revoked.store(true, Ordering::Release);
                    local
                        .indicator()
                        .immediate_revoke(owner.check().unwrap(), StopReason::AuthorityEnded);
                }
                nonce(&mut n)
            },
        );
        let peer = async {
            let selected = select(&mut viewer).await;
            chosen.store(true, Ordering::Release);
            Box::pin(accept_selected(
                viewer,
                selected,
                c,
                h,
                control.clone(),
                selection,
            ))
            .await
        };
        let error = Box::pin(refusal(opening, peer)).await;
        assert!(revoked.load(Ordering::Acquire));
        assert!(matches!(error, PublishError::Shared(_)));
        assert!(control.check().is_err());
        assert!(owner.check().is_err());
        let report = local
            .service_shared_sources(|| panic!("registration revoked synchronously"))
            .unwrap();
        assert_eq!(report.outcomes().count(), 0);
        drop(initial);
        close(&mut publisher, vec![]).await;
    });
}

#[test]
fn pre_viewer_entropy_failure_and_unwind_retire_the_original_source() {
    let rt = support::runtime();
    rt.block_on(async {
        for mode in 0..3 {
            let (mut publisher, owner, initial) = selected_publisher(&rt).await;
            let mut local = agent(true);
            local.attach_shared_source(&publisher).unwrap();
            if mode == 2 {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    local.service_shared_sources(|| panic!("deliberate local entropy unwind"))
                }));
                assert!(result.is_err());
            } else {
                let report = local
                    .service_shared_sources(|| if mode == 0 { Ok(0) } else { Err(()) })
                    .unwrap();
                assert_eq!(
                    *report.outcomes().next().unwrap(),
                    Err(ConsentError::NonceUnavailable)
                );
                assert_eq!(report.next_deadline(), None);
            }
            assert!(owner.check().is_err());
            assert!(publisher.join_queue().selected_catalog().is_err());
            drop(initial);
            close(&mut publisher, vec![]).await;
        }
    });
}

#[test]
fn pre_viewer_renewal_callback_is_lock_free_and_cannot_resurrect_a_closed_source() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut local = agent(true);
        local.attach_shared_source(&publisher).unwrap();
        let report = local
            .service_shared_sources(|| {
                publisher.close(); // Would deadlock if a policy lock covered the callback.
                Ok(91001)
            })
            .unwrap();
        assert!(report.outcomes().next().unwrap().is_err());
        assert!(owner.check().is_err());
        drop(initial);
        close(&mut publisher, vec![]).await;
    });
}

#[test]
fn independently_selected_pending_sources_share_only_the_agents_permission_owner() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut a, a_owner, a_initial) = selected_publisher(&rt).await;
        let (mut b, b_owner, b_initial) = selected_publisher(&rt).await;
        assert_ne!(a.worker_id(), b.worker_id());
        let mut local = agent(true);
        local.attach_shared_source(&a).unwrap();
        local.attach_shared_source(&b).unwrap();
        let mut n = 101_000;
        let report = local.service_shared_sources(|| nonce(&mut n)).unwrap();
        assert_eq!(report.outcomes().count(), 2);
        assert!(report.outcomes().all(|s| s.as_ref().unwrap().renewed));
        a.close();
        assert!(a_owner.check().is_err());
        assert!(b_owner.check().is_ok());
        let report = local
            .service_shared_sources(|| panic!("b not due"))
            .unwrap();
        assert_eq!(report.outcomes().filter(|s| s.is_ok()).count(), 1);
        assert_eq!(b.tick().unwrap(), 0);
        local.on_os_locked(HostInstant::from_micros(
            now(&Cx::current().unwrap()).unwrap(),
        ));
        assert!(b_owner.check().is_err());
        drop((a_initial, b_initial));
        close(&mut a, vec![]).await;
        close(&mut b, vec![]).await;
    });
}

#[test]
fn pre_viewer_registration_renews_the_running_selected_source_past_its_original_deadline() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let original_until = owner.deadline(Duration::from_secs(5)).unwrap().time();
        let original_worker = publisher.worker_id();
        let mut local = agent(true);
        local.attach_shared_source(&publisher).unwrap();
        let mut peer = Box::pin(first(&rt, &mut publisher, &initial, 13)).await;
        let mut captures = 0;
        let mut renewals = 0;
        let mut n = 121_000;
        let source = publisher.serve(Duration::from_millis(50), |_| captures += 1);
        let network = async {
            loop {
                let status = service(&mut local, nonce(&mut n).unwrap()).unwrap();
                renewals += usize::from(status.renewed);
                peer.peer.turn().await.unwrap();
                assert!(!owner.view_ready().unwrap());
                assert!(!peer.peer.control.view_ready().unwrap());
                if peer.peer.h.timer_driver().unwrap().now() > original_until {
                    break;
                }
            }
            assert!(owner.check().is_ok(), "only local checks renew this source");
            assert!(
                peer.peer.control.check().is_ok(),
                "original session renews independently"
            );
            assert_eq!(
                peer.peer.frames,
                [0],
                "idle observations do not encode dummy pictures"
            );
            let (_, releases) = local.on_screen_capture_revoked(owner.check().unwrap());
            assert_eq!(releases, []);
            assert!(owner.check().is_err());
            assert!(peer.peer.control.check().is_err());
        };
        let (result, ()) = Box::pin(support::both(source, network)).await;
        assert!(matches!(
            result,
            Err(crate::media::shared_publisher::Error::Closed)
        ));
        assert!(captures > 10);
        assert!(renewals >= 4);
        assert_eq!(publisher.worker_id(), original_worker);
        drop(initial);
        close(&mut publisher, vec![peer]).await;
    });
}

async fn managed_first(
    rt: &Runtime,
    local: &mut SessionAgent,
    publisher: &mut Publisher,
    initial: &SharedCaptureUpdate,
    id: u128,
) -> SelectedPeer {
    let (c, h, host, viewer) =
        sessions(rt, id, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
    let selection = host.selection().clone();
    let control = host.original_observation();
    let mut n = 131_000;
    let opening = local.start_shared_display(
        host,
        publisher,
        initial,
        Duration::from_secs(2),
        SendPolicy::default(),
        || nonce(&mut n),
    );
    is_send(&opening);
    // This must compile and remain operable while the network future is held:
    // source renewal/revocation never lends the local agent to the media task.
    service(local, 141_001).unwrap();
    let (shared, mut peer) = Box::pin(support::both(
        opening,
        accept(viewer, c, h, control, selection),
    ))
    .await;
    peer.peer.shared = Some(shared.unwrap());
    while !peer.peer.complete() {
        peer.peer.turn().await.unwrap();
    }
    peer
}

#[test]
fn managed_startup_registers_local_consent_without_manual_attachment_or_agent_borrow() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let pid = publisher.worker_id();
        let mut local = agent(true);
        let peer = Box::pin(managed_first(&rt, &mut local, &mut publisher, &initial, 13)).await;
        assert_eq!(publisher.worker_id(), pid);
        assert!(!peer.peer.control.view_ready().unwrap());
        let report = local.service_shared_sources(|| Ok(141_002)).unwrap();
        assert_eq!(report.outcomes().count(), 1);
        drop(local);
        assert!(owner.check().is_err());
        assert!(peer.peer.control.check().is_err());
        drop(initial);
        close(&mut publisher, vec![peer]).await;
    });
}

#[test]
fn managed_first_viewer_retry_reuses_only_the_original_agents_source_registration() {
    use crate::session_agent::source::StartError;
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, initial) = selected_publisher(&rt).await;
        let mut local = agent(true);
        let (_, _, host, _viewer) =
            sessions(&rt, 13, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let first_control = host.original_observation();
        drop(local.start_shared_display(
            host,
            &mut publisher,
            &initial,
            Duration::from_secs(2),
            SendPolicy::default(),
            || panic!("unpolled call never needs entropy"),
        ));
        assert!(first_control.check().is_err());
        assert!(owner.check().is_ok());
        let before = service(&mut local, 151_001).unwrap();
        let mut foreign = agent(true);
        let (_, _, host, _viewer) =
            sessions(&rt, 14, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
        let foreign_control = host.original_observation();
        let denied = foreign.start_shared_display(
            host,
            &mut publisher,
            &initial,
            Duration::from_secs(2),
            SendPolicy::default(),
            || panic!("foreign agent cannot open channels"),
        );
        assert!(foreign_control.check().is_err(), "call-time refusal");
        assert!(matches!(
            denied.await,
            Err(StartError::Consent(ConsentError::AlreadyAttached))
        ));
        assert!(owner.check().is_ok());
        let peer = Box::pin(managed_first(&rt, &mut local, &mut publisher, &initial, 15)).await;
        let after = service(&mut local, 151_002).unwrap();
        assert_eq!(after.authorized_until, before.authorized_until);
        assert_eq!(after.next_check, before.next_check);
        assert!(!after.renewed);
        drop(foreign);
        assert!(owner.check().is_ok());
        drop(initial);
        close(&mut publisher, vec![peer]).await;
    });
}

#[test]
fn managed_startup_permission_refusal_is_synchronous_and_does_not_steal_a_source() {
    use crate::session_agent::source::StartError;
    let rt = support::runtime();
    rt.block_on(async {
        for previously_owned in [false, true] {
            let (mut publisher, owner, initial) = selected_publisher(&rt).await;
            let mut local = agent(previously_owned);
            if previously_owned {
                local.attach_shared_source(&publisher).unwrap();
                local
                    .permissions_mut()
                    .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Denied);
            }
            let (_, _, host, _viewer) =
                sessions(&rt, 13, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
            let control = host.original_observation();
            let opening = local.start_shared_display(
                host,
                &mut publisher,
                &initial,
                Duration::from_secs(2),
                SendPolicy::default(),
                || panic!("permission failure cannot request a ticket"),
            );
            assert!(
                control.check().is_err(),
                "refusal happens before first poll"
            );
            assert!(matches!(
                opening.await,
                Err(StartError::Consent(ConsentError::NoCapturePermission))
            ));
            assert_eq!(owner.check().is_err(), previously_owned);
            drop(initial);
            close(&mut publisher, vec![]).await;
        }
    });
}

#[test]
fn managed_opening_does_not_keep_the_local_agent_alive_or_block_its_revoke_event() {
    let rt = support::runtime();
    rt.block_on(async {
        for drop_agent in [false, true] {
            let (mut publisher, owner, initial) = selected_publisher(&rt).await;
            let mut local = agent(true);
            let (_, _, host, _viewer) =
                sessions(&rt, 13, Role::Observe, &[fr_wire::display::CAPABILITY]).await;
            let control = host.original_observation();
            let opening = local.start_shared_display(
                host,
                &mut publisher,
                &initial,
                Duration::from_secs(2),
                SendPolicy::default(),
                || panic!("source revoked before network starts"),
            );
            is_send(&opening);
            if drop_agent {
                drop(local);
            } else {
                local.on_os_locked(owner.check().unwrap());
            }
            assert!(owner.check().is_err());
            assert!(opening.await.is_err());
            assert!(control.check().is_err());
            drop(initial);
            close(&mut publisher, vec![]).await;
        }
    });
}
