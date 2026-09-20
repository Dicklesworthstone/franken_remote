//! Real publisher/native IPC and TLS/UDP peers; cached permission transitions and
//! codec payloads are fixtures, not platform permission or HEVC qualification.
use super::*;
use fr_core::{
    input::{DesktopPoint, InputBounds},
    time::HostInstant,
};
use frd::{
    input_watchdog::StopReason,
    session_agent::{
        ApprovalMode, PermissionKind, PermissionStatus, PlatformKind, SessionAgent,
        source::{Error as ConsentError, Status},
    },
};

fn agent(granted: bool) -> SessionAgent {
    let mut agent = SessionAgent::new(
        ApprovalMode::PromptAlways,
        PlatformKind::LinuxX11,
        12,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
    );
    if granted {
        // Model a completed local OS permission probe. This is not an OS grant.
        agent
            .permissions_mut()
            .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
    }
    agent
}
fn service(agent: &mut SessionAgent, nonce: u128) -> Result<Status, ConsentError> {
    let report = agent.service_shared_sources(|| Ok(nonce)).unwrap();
    assert_eq!(report.outcomes().count(), 1);
    *report.outcomes().next().unwrap()
}
fn fenced(cohort: &Cohort) {
    assert!(cohort.owner.check().is_err());
    for peer in &cohort.peers {
        assert!(peer.control.check().is_err());
    }
}
async fn sleep_ms(cx: &Cx, ms: u64) {
    sleep(cx.timer_driver().unwrap().now(), Duration::from_millis(ms)).await;
}
fn renew_viewer(peer: &Peer, nonce: u128) {
    // The fixture separately services each original viewer authority. This is
    // not the source's challenge and cannot extend the source's deadline.
    let fixed = peer.control.issue_challenge(nonce).unwrap();
    assert_eq!(peer.control.renew(nonce).unwrap(), fixed);
}

#[test]
fn local_source_renewal_sustains_original_capture_past_its_initial_permission_deadline() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let pid = cohort.publisher.worker_id();
        let first_until = cohort
            .owner
            .deadline(Duration::from_secs(5))
            .unwrap()
            .time();
        let readiness = [
            cohort.peers[0].control.view_ready().unwrap(),
            cohort.peers[1].control.view_ready().unwrap(),
        ];
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        let mut frame = 0;
        let mut renewals = 0;
        let mut nonce = 1000;
        let mut continued_after_departure = false;
        loop {
            nonce += 1;
            let status = service(&mut local, nonce).unwrap();
            renewals += usize::from(status.renewed);
            assert!(status.next_check < status.authorized_until);
            assert!(status.authorized_until.as_micros() <= clock(&cx) + 5_000_000);
            for (i, peer) in cohort.peers.iter().enumerate() {
                if peer.subscriber.is_some() {
                    renew_viewer(peer, nonce + 10_000);
                    assert_eq!(peer.control.view_ready().unwrap(), readiness[i]);
                }
            }
            let captured = cohort.publisher.capture_next().await.unwrap();
            frame += 1;
            assert_eq!(captured.frame, frame);
            for peer in &mut cohort.peers {
                if peer.subscriber.is_some() {
                    deliver(peer, &cx, frame).await;
                }
            }
            if renewals >= 2 && cohort.peers[0].subscriber.is_some() {
                drop(cohort.peers[0].subscriber.take());
                assert!(cohort.peers[0].control.check().is_err());
                cohort.peers[1].control.check().unwrap();
            } else if cohort.peers[0].subscriber.is_none() {
                continued_after_departure = true;
            }
            if cx.timer_driver().unwrap().now() > first_until {
                break;
            }
            sleep_ms(&cx, 100).await;
        }
        assert!(renewals >= 4);
        assert!(continued_after_departure);
        assert_eq!(cohort.publisher.worker_id(), pid);
        assert_eq!(cohort.publisher.tick().unwrap(), 1);
        stop_cohort(&mut cohort, &cx).await;
        assert!(service(&mut local, nonce + 1).is_err());
    });
}

#[test]
fn unknown_capture_permission_and_duplicate_local_owners_cannot_change_a_live_publisher() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut unknown = agent(false);
        assert_eq!(
            unknown.attach_shared_source(&cohort.publisher),
            Err(ConsentError::NoCapturePermission)
        );
        cohort.owner.check().unwrap();
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        assert_eq!(
            local.attach_shared_source(&cohort.publisher),
            Err(ConsentError::AlreadyAttached)
        );
        let mut foreign = agent(true);
        assert_eq!(
            foreign.attach_shared_source(&cohort.publisher),
            Err(ConsentError::AlreadyAttached)
        );
        drop((unknown, foreign));
        assert!(service(&mut local, 77).unwrap().renewed);
        assert_eq!(cohort.publisher.capture_next().await.unwrap().delivered, 2);
        for peer in &mut cohort.peers {
            deliver(peer, &cx, 1).await;
        }
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn repeated_maintenance_does_not_consume_entropy_or_move_the_original_renewal_deadline() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, false, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        let first = service(&mut local, 50).unwrap();
        assert!(first.renewed);
        for _ in 0..2000 {
            let report = local
                .service_shared_sources(|| panic!("renewal is not due"))
                .unwrap();
            let status = report.outcomes().next().unwrap().as_ref().unwrap();
            assert!(!status.renewed);
            assert_eq!(status.authorized_until, first.authorized_until);
            assert_eq!(report.next_deadline(), Some(first.next_check));
        }
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn local_indicator_revoke_fences_every_viewer_before_pending_native_capture_completes() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, true)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        service(&mut local, 81).unwrap();
        let pid = cohort.publisher.worker_id();
        {
            let mut capture = pin!(cohort.publisher.capture_next());
            poll_fn(|task| match capture.as_mut().poll(task) {
                Poll::Pending => Poll::Ready(()),
                Poll::Ready(_) => panic!("real delayed child should remain pending"),
            })
            .await;
            local.indicator().immediate_revoke(
                HostInstant::from_micros(clock(&cx)),
                StopReason::AuthorityEnded,
            );
            assert!(cohort.owner.check().is_err());
            for peer in &cohort.peers {
                assert!(peer.control.check().is_err());
            }
            assert!(capture.await.is_err());
        }
        assert_eq!(cohort.publisher.worker_id(), pid);
        let report = local
            .service_shared_sources(|| panic!("indicator removed all renewal registrations"))
            .unwrap();
        assert_eq!(report.outcomes().count(), 0);
        assert_eq!(
            local.attach_shared_source(&cohort.publisher),
            Err(ConsentError::Closed)
        );
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn lock_and_screen_capture_revocation_and_agent_drop_are_terminal_without_a_poll() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        for reason in 0..4 {
            let mut cohort = Box::pin(joined(&rt, true, false)).await;
            let mut local = agent(true);
            local.attach_shared_source(&cohort.publisher).unwrap();
            match reason {
                0 => local.on_os_locked(HostInstant::from_micros(clock(&cx))),
                1 => {
                    let (_, cleanup) =
                        local.on_screen_capture_revoked(HostInstant::from_micros(clock(&cx)));
                    assert_eq!(cleanup, []);
                }
                2 => local.on_os_session_changed(13, HostInstant::from_micros(clock(&cx))),
                _ => {
                    drop(local);
                    fenced(&cohort);
                    stop_cohort(&mut cohort, &cx).await;
                    continue;
                }
            }
            fenced(&cohort);
            // Restoring a permission bit is NOT a new local authority grant.
            local
                .permissions_mut()
                .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Granted);
            assert!(local.attach_shared_source(&cohort.publisher).is_err());
            fenced(&cohort);
            stop_cohort(&mut cohort, &cx).await;
        }
    });
}

#[test]
fn failed_or_reused_entropy_fences_the_source_instead_of_inventing_a_challenge() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        for nonce in [Ok(0), Err(())] {
            let mut cohort = Box::pin(joined(&rt, true, false)).await;
            let mut local = agent(true);
            local.attach_shared_source(&cohort.publisher).unwrap();
            let report = local.service_shared_sources(|| nonce).unwrap();
            assert_eq!(
                *report.outcomes().next().unwrap(),
                Err(ConsentError::NonceUnavailable)
            );
            assert_eq!(report.next_deadline(), None);
            fenced(&cohort);
            stop_cohort(&mut cohort, &cx).await;
        }
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        service(&mut local, 90).unwrap();
        sleep_ms(&cx, 1010).await;
        assert_eq!(service(&mut local, 90), Err(ConsentError::NonceUnavailable));
        fenced(&cohort);
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn caller_entropy_runs_outside_publisher_locks_and_revocation_wins_before_renewal() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        let report = local
            .service_shared_sources(|| {
                // Would deadlock if renewal held the publisher policy mutex.
                cohort.publisher.close();
                Ok(444)
            })
            .unwrap();
        assert!(report.outcomes().next().unwrap().is_err());
        fenced(&cohort);
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn cached_permission_loss_is_checked_even_when_no_renewal_is_due() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        service(&mut local, 100).unwrap();
        local
            .permissions_mut()
            .set_permission(PermissionKind::ScreenCapture, PermissionStatus::Denied);
        assert_eq!(
            service(&mut local, 101),
            Err(ConsentError::NoCapturePermission)
        );
        fenced(&cohort);
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn renewed_viewers_cannot_resurrect_an_expired_source_after_local_service_stalls() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        let until = service(&mut local, 1000)
            .unwrap()
            .authorized_until
            .as_micros();
        let mut nonce = 2000;
        while clock(&cx) <= until {
            nonce += 1;
            for peer in &cohort.peers {
                renew_viewer(peer, nonce);
            }
            sleep_ms(&cx, 100).await;
        }
        for peer in &cohort.peers {
            peer.control.check().unwrap();
        }
        let report = local
            .service_shared_sources(|| panic!("expired source cannot issue new challenges"))
            .unwrap();
        assert!(report.outcomes().next().unwrap().is_err());
        fenced(&cohort);
        assert!(service(&mut local, 9999).is_err());
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn weak_registry_cannot_retain_a_dropped_publisher() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let cohort = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        let Cohort {
            publisher,
            mut peers,
            owner,
        } = *cohort;
        drop(publisher);
        assert!(owner.check().is_err());
        assert_eq!(service(&mut local, 5), Err(ConsentError::Closed));
        for peer in &mut peers {
            assert!(peer.control.check().is_err());
            assert!(peer.service(&cx, 1).is_err());
            reap(&mut peer.presenter, &cx).await;
        }
    });
}

#[test]
fn a_local_agent_bounds_source_registrations_and_reuses_only_retired_slots() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut local = agent(true);
        let mut cohorts = Vec::new();
        for _ in 0..frd::session_agent::source::MAX_SOURCES {
            let cohort = Box::pin(joined(&rt, true, false)).await;
            local.attach_shared_source(&cohort.publisher).unwrap();
            // Other independently authorized sources may already be running;
            // renew their original permissions, not a new blanket allowance.
            let mut nonce = 20_000 + u128::try_from(cohorts.len()).unwrap() * 100;
            local
                .service_shared_sources(|| {
                    nonce += 1;
                    Ok(nonce)
                })
                .unwrap();
            cohorts.push(cohort);
        }
        let mut last = Box::pin(joined(&rt, true, false)).await;
        assert_eq!(
            local.attach_shared_source(&last.publisher),
            Err(ConsentError::Full)
        );
        last.owner.check().unwrap();
        assert_eq!(last.publisher.tick().unwrap(), 2);
        stop_cohort(&mut cohorts[0], &cx).await;
        local.attach_shared_source(&last.publisher).unwrap();
        assert_eq!(
            local
                .service_shared_sources(|| Ok(99_999))
                .unwrap()
                .outcomes()
                .count(),
            frd::session_agent::source::MAX_SOURCES
        );
        // Source registration teardown does not create a new encoder or allow a
        // stale handle to reach the source that later occupied its registry slot.
        assert!(cohorts[0].owner.check().is_err());
        last.owner.check().unwrap();
        for cohort in &mut cohorts[1..] {
            stop_cohort(cohort, &cx).await;
        }
        stop_cohort(&mut last, &cx).await;
    });
}

#[test]
fn one_source_retiring_does_not_revoke_another_sources_local_permission() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut a = Box::pin(joined(&rt, true, false)).await;
        let mut b = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&a.publisher).unwrap();
        local.attach_shared_source(&b.publisher).unwrap();
        let mut nonce = 100;
        let report = local
            .service_shared_sources(|| {
                nonce += 1;
                Ok(nonce)
            })
            .unwrap();
        assert_eq!(report.outcomes().count(), 2);
        assert!(report.outcomes().all(Result::is_ok));
        a.publisher.close();
        let report = local
            .service_shared_sources(|| panic!("second source renewal is not yet due"))
            .unwrap();
        let outcomes: Vec<_> = report.outcomes().collect();
        assert!(outcomes[0].is_err());
        assert!(outcomes[1].is_ok());
        b.owner.check().unwrap();
        assert_eq!(b.publisher.capture_next().await.unwrap().delivered, 2);
        for peer in &mut b.peers {
            deliver(peer, &cx, 1).await;
        }
        stop_cohort(&mut a, &cx).await;
        stop_cohort(&mut b, &cx).await;
    });
}

#[test]
fn interrupted_local_renewal_fences_its_source_before_unwinding_to_the_event_loop() {
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            local.service_shared_sources(|| panic!("test-only interrupted entropy provider"))
        }));
        assert!(panic.is_err());
        fenced(&cohort);
        assert!(service(&mut local, 700).is_err());
        stop_cohort(&mut cohort, &cx).await;
    });
}

#[test]
fn capture_permission_loss_returns_held_input_cleanup_without_claiming_os_completion() {
    use fr_core::{
        input::{KeyTransition, PhysicalKey, PointerButton},
        input_submission::Operation,
    };
    let rt = runtime();
    rt.block_on(async {
        let cx = Cx::current().unwrap();
        let mut cohort = Box::pin(joined(&rt, true, false)).await;
        let mut local = agent(true);
        local.attach_shared_source(&cohort.publisher).unwrap();
        let shift = PhysicalKey::new(0xe1).unwrap();
        // Model already-submitted remote effects, never synthesize a new grant.
        local
            .held_state_mut()
            .record_injected_operation(&Operation::Key {
                key: shift,
                transition: KeyTransition::Press,
            });
        local
            .held_state_mut()
            .record_injected_operation(&Operation::Button {
                button: PointerButton::Primary,
                pressed: true,
            });
        let (outcome, releases) =
            local.on_screen_capture_revoked(HostInstant::from_micros(clock(&cx)));
        fenced(&cohort);
        assert!(!outcome.os_cleanup.is_complete());
        assert_eq!(
            releases,
            [
                Operation::Button {
                    button: PointerButton::Primary,
                    pressed: false
                },
                Operation::Key {
                    key: shift,
                    transition: KeyTransition::Release
                },
            ]
        );
        stop_cohort(&mut cohort, &cx).await;
    });
}
