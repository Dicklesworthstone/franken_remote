//! Real TLS/UDP and production incoming owners. Identity, permission, monitor,
//! coded pictures and decode acknowledgements are explicit test fixtures.
use super::*;
use crate::session_startup::{Approval, Error as OpenError};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::Mutex,
};
mod client;
use client::Client;

struct Fresh {
    c: Cx,
    h: Cx,
    host: Host,
    viewer: Viewer,
    alive: Arc<AtomicBool>,
}
async fn fresh(
    rt: &Runtime,
    id: u128,
    approval: bool,
    role: Role,
    timeout: Duration,
) -> Box<Fresh> {
    let c = rt.request_cx_with_budget(Budget::INFINITE);
    let h = rt.request_cx_with_budget(Budget::INFINITE);
    let mut capabilities: Vec<_> = [
        decoder::CAPABILITY,
        attachment::CAPABILITY,
        attachment::DELIVERY_CAPABILITY,
        fr_wire::display::CAPABILITY,
    ]
    .into_iter()
    .map(|name| Capability {
        name: name.into(),
        version: 1,
        required: true,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    let offer = Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
    };
    let config = Configuration {
        offer: offer.clone(),
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(id),
        },
        require_approval: approval,
        startup_timeout: timeout,
        authority: AuthorityPolicy::plan_defaults(),
        transport: Policy {
            critical_send_records: 1,
            ..Policy::default()
        },
    };
    let (client, native) = support::native_pair(&c, "localhost", quic::ALPN).await;
    let alive = Arc::new(AtomicBool::new(true));
    let peer = Peer::Fixture {
        alive: alive.clone(),
        until: now(&h).unwrap() + 30_000_000,
        control: true,
    };
    let viewer = Viewer::new(
        c.clone(),
        client.unwrap(),
        Offer { role, ..offer },
        config.transport,
        Duration::from_secs(4),
    )
    .unwrap();
    let host = Host::start(h.clone(), native.unwrap(), peer, config).unwrap();
    Box::new(Fresh {
        c,
        h,
        host,
        viewer,
        alive,
    })
}
async fn prompt(viewer: &mut Viewer, local: &Mutex<Option<Approval>>) {
    while local.lock().unwrap().is_none() {
        viewer.drive(Duration::from_millis(1)).await.unwrap();
    }
    assert!(
        !viewer.is_complete(),
        "a notification is not observation consent"
    );
}
async fn finished(viewer: &mut Viewer, ticket: &Ticket) {
    // The host closes independently; a peer read may fail after that close.
    for _ in 0..2000 {
        if matches!(ticket.state(), State::Finished(_)) {
            return;
        }
        let _ = viewer.drive(Duration::from_millis(1)).await;
        asupersync::runtime::yield_now().await;
    }
    panic!("incoming owner failed to retire: {:?}", ticket.state());
}

#[test]
fn incoming_host_approval_and_display_join_run_in_one_slot_without_pausing_healthy_media() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let pid = publisher.worker_id();
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let Fresh {
                    c,
                    h,
                    host,
                    mut viewer,
                    alive,
                } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
                let local = Arc::new(Mutex::new(None));
                let calls = Arc::new(AtomicU64::new(0));
                let (copy, count, reentrant) = (local.clone(), calls.clone(), admission.clone());
                let ticket = admission
                    .admit_host(host, move |a, role| {
                        assert_eq!(role, Role::Observe);
                        assert_eq!(reentrant.statistics().unwrap().admitted, 2);
                        count.fetch_add(1, Ordering::Relaxed);
                        *copy.lock().unwrap() = Some(a);
                        Ok(())
                    })
                    .unwrap();
                assert_eq!(ticket.state(), State::Opening);
                prompt(&mut viewer, &local).await;
                for _ in 0..8 {
                    viewer.drive(Duration::from_millis(1)).await.unwrap();
                }
                assert!(!viewer.is_complete());
                assert!(viewer.approval().is_some());
                assert_eq!(ticket.state(), State::Opening);
                assert_eq!(calls.load(Ordering::Relaxed), 1);
                let decision = local.lock().unwrap().take().unwrap();
                decision.decide(true).unwrap();
                let mut client = Box::pin(Client::start(c, viewer)).await;
                client.ready().await;
                assert_eq!(ticket.state(), State::Serving);
                assert_ne!(client.frames, [] as [u64; 0]);
                assert_eq!(calls.load(Ordering::Relaxed), 1);
                assert_eq!(admission.statistics().unwrap().admitted, 2);
                assert!(decision.decide(true).is_err());
                assert!(h.checkpoint().is_ok());
                assert!(alive.load(Ordering::Acquire));
                assert!(owner.check().is_ok());
                assert_eq!(initial.state(), State::Serving);
                ticket.close();
                assert!(
                    h.checkpoint().is_err(),
                    "cancel the original session, not only its slot"
                );
                for _ in 0..5 {
                    asupersync::runtime::yield_now().await;
                }
                client.viewer.close();
            }),
        ))
        .await;
        assert_eq!(publisher.worker_id(), pid);
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_unattended_observation_runs_without_inventing_local_approval() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, _owner, mut first, mut hub, admission, _) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let Fresh {
                    c, host, viewer, ..
                } = *Box::pin(fresh(&rt, 14, false, Role::Observe, Duration::from_secs(2))).await;
                let ticket = admission
                    .admit_host(host, |_, _| {
                        panic!("unattended local policy does not request approval")
                    })
                    .unwrap();
                let mut client = Box::pin(Client::start(c, viewer)).await;
                client.ready().await;
                assert_eq!(ticket.state(), State::Serving);
                assert_ne!(client.frames, [] as [u64; 0]);
                ticket.close();
                client.viewer.close();
            }),
        ))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_slots_count_parked_handshakes_and_reject_duplicate_or_foreign_owners() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _first, mut hub, admission, _) = Box::pin(fixture(
            &rt,
            service::Policy {
                viewers: 2,
                ..service::Policy::default()
            },
            entropy(),
        ))
        .await;
        let old = Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
        let old_h = old.h.clone();
        let old_alive = old.alive.clone();
        let ticket = admission
            .admit_host(old.host, |_, _| panic!("not polled"))
            .unwrap();
        for (id, scope, expected) in [
            (15, 12, service::Error::Full),
            (14, 12, service::Error::DuplicateSession),
            (15, 99, service::Error::ForeignScope),
        ] {
            let mut attempt =
                Box::pin(fresh(&rt, id, true, Role::Observe, Duration::from_secs(2))).await;
            attempt.host.config.binding.os_session = OsSessionId::from_raw(scope);
            let alive = attempt.alive.clone();
            assert_eq!(
                admission
                    .admit_host(attempt.host, |_, _| panic!("refused before callback"))
                    .unwrap_err(),
                expected
            );
            assert!(!alive.load(Ordering::Acquire));
        }
        assert_eq!(admission.statistics().unwrap().admitted, 2);
        assert!(owner.check().is_ok());
        assert_eq!(ticket.state(), State::Opening);
        drop(hub.serve());
        assert!(old_h.checkpoint().is_err());
        assert!(!old_alive.load(Ordering::Acquire));
        assert!(matches!(ticket.state(), State::Finished(Err(_))));
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_cancelled_prompt_cannot_approve_reused_session_number() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, _) = Box::pin(fixture(
            &rt,
            service::Policy {
                viewers: 2,
                ..service::Policy::default()
            },
            entropy(),
        ))
        .await;
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let Fresh {
                    h,
                    host,
                    mut viewer,
                    alive,
                    ..
                } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
                let local = Arc::new(Mutex::new(None));
                let copy = local.clone();
                let old = admission
                    .admit_host(host, move |a, _| {
                        *copy.lock().unwrap() = Some(a);
                        Ok(())
                    })
                    .unwrap();
                prompt(&mut viewer, &local).await;
                old.close();
                let approval = local.lock().unwrap().take().unwrap();
                assert!(approval.decide(true).is_err());
                assert!(h.checkpoint().is_err());
                while admission.statistics().unwrap().finished == 0 {
                    asupersync::runtime::yield_now().await;
                }
                assert!(!alive.load(Ordering::Acquire));
                let Fresh {
                    c, h, host, viewer, ..
                } = *Box::pin(fresh(&rt, 14, false, Role::Observe, Duration::from_secs(2))).await;
                let replacement = admission.admit_host(host, |_, _| unreachable!()).unwrap();
                old.close();
                assert!(h.checkpoint().is_ok());
                let mut client = Box::pin(Client::start(c, viewer)).await;
                client.ready().await;
                assert_eq!(replacement.state(), State::Serving);
                assert!(owner.check().is_ok());
                assert_eq!(admission.statistics().unwrap().admitted, 3);
                replacement.close();
            }),
        ))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_refuses_control_before_prompt_or_observation_instead_of_downgrading() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, _) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let Fresh {
                    host,
                    mut viewer,
                    alive,
                    ..
                } = *Box::pin(fresh(
                    &rt,
                    14,
                    true,
                    Role::RequestControl,
                    Duration::from_secs(2),
                ))
                .await;
                let ticket = admission
                    .admit_host(host, |_, _| panic!("control must refuse before prompt"))
                    .unwrap();
                finished(&mut viewer, &ticket).await;
                assert_eq!(
                    ticket.state(),
                    State::Finished(Err(service::Error::Session(OpenError::Denied)))
                );
                assert!(!viewer.is_complete());
                assert!(viewer.approval().is_none());
                assert!(!alive.load(Ordering::Acquire));
                assert!(owner.check().is_ok());
            }),
        ))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_original_deadline_includes_queue_time_and_approval_to_attachment_handoff() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, _) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        // Let an unpolled admission expire while the established peer is serviced.
        let Fresh {
            host,
            mut viewer,
            alive,
            ..
        } = *Box::pin(fresh(
            &rt,
            14,
            true,
            Role::Observe,
            Duration::from_millis(20),
        ))
        .await;
        let until = host.deadline_us();
        let clock = owner.context();
        let parked = admission
            .admit_host(host, |_, _| {
                panic!("expired parked admission cannot notify")
            })
            .unwrap();
        asupersync::time::sleep_until(asupersync::types::Time::from_nanos(until * 1000)).await;
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                finished(&mut viewer, &parked).await;
                assert!(matches!(
                    parked.state(),
                    State::Finished(Err(service::Error::Session(OpenError::Expired)))
                ));
                assert!(!alive.load(Ordering::Acquire));
                let Fresh {
                    host,
                    mut viewer,
                    h,
                    ..
                } = *Box::pin(fresh(
                    &rt,
                    15,
                    true,
                    Role::Observe,
                    Duration::from_millis(350),
                ))
                .await;
                let until = host.deadline_us();
                let local = Arc::new(Mutex::new(None));
                let copy = local.clone();
                let ticket = admission
                    .admit_host(host, move |a, _| {
                        *copy.lock().unwrap() = Some(a);
                        Ok(())
                    })
                    .unwrap();
                prompt(&mut viewer, &local).await;
                local.lock().unwrap().take().unwrap().decide(true).unwrap();
                while !viewer.is_complete() {
                    viewer.drive(Duration::from_millis(1)).await.unwrap();
                }
                let mut session = viewer.finish().unwrap();
                // Intentionally never select the offered display. This must end at
                // the ORIGINAL Host cutoff, not a new two-second post-approval lease.
                while !matches!(ticket.state(), State::Finished(_)) {
                    let _ = session.drive(Duration::from_millis(1), block).await;
                    assert!(
                        now(&clock).unwrap() < until + 100_000,
                        "approval reset startup budget"
                    );
                }
                assert!(h.checkpoint().is_err());
                assert!(owner.check().is_ok());
            }),
        ))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_denial_and_notification_failure_retire_only_their_attempt() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, _) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                for deny in [true, false] {
                    let Fresh {
                        host,
                        mut viewer,
                        alive,
                        ..
                    } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2)))
                        .await;
                    let ticket = admission
                        .admit_host(host, move |a, _| {
                            if deny {
                                a.decide(false).unwrap();
                                Ok(())
                            } else {
                                Err(())
                            }
                        })
                        .unwrap();
                    finished(&mut viewer, &ticket).await;
                    assert_eq!(
                        ticket.state(),
                        State::Finished(Err(service::Error::Session(OpenError::Denied)))
                    );
                    assert!(!viewer.is_complete());
                    assert!(!alive.load(Ordering::Acquire));
                    assert!(owner.check().is_ok());
                }
                assert_eq!(admission.statistics().unwrap().finished, 2);
            }),
        ))
        .await;
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_notification_unwind_fences_before_retained_service_future_is_dropped() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            mut viewer,
            h,
            alive,
            ..
        } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
        let notice = Arc::new(Mutex::new(None));
        let copy = notice.clone();
        let ticket = admission
            .admit_host(host, move |a, _| {
                *copy.lock().unwrap() = Some(a);
                panic!("local notification fixture panic");
            })
            .unwrap();
        let mut run = Box::pin(hub.serve());
        let mut capture = Box::pin(publisher.serve(Duration::from_millis(50), |_| {}));
        let mut healthy = Box::pin(async {
            loop {
                client_turn(&mut first.peer).await;
            }
        });
        let mut client = Box::pin(async {
            loop {
                let _ = viewer.drive(Duration::from_millis(1)).await;
            }
        });
        poll_fn(|task| {
            match catch_unwind(AssertUnwindSafe(|| run.as_mut().poll(task))) {
                Ok(result) => assert!(result.is_pending(), "hub stopped: {result:?}"),
                Err(_) => return Poll::Ready(()),
            }
            assert!(capture.as_mut().poll(task).is_pending());
            assert!(healthy.as_mut().poll(task).is_pending());
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        // A panicked service is terminal, not a retryable poll. Retain it here:
        // both original authorities must already be fenced, not only on Drop.
        assert!(h.checkpoint().is_err());
        assert!(!alive.load(Ordering::Acquire));
        assert!(notice.lock().unwrap().take().unwrap().decide(true).is_err());
        assert!(matches!(ticket.state(), State::Finished(Err(_))));
        assert!(matches!(initial.state(), State::Finished(Err(_))));
        assert!(owner.check().is_err());
        drop(client);
        assert!(!viewer.is_complete());
        drop(healthy);
        drop(capture);
        drop(run);
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_source_revoked_inside_approval_cannot_publish_observation() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, _first, mut hub, admission, initial) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        let Fresh {
            host,
            mut viewer,
            alive,
            ..
        } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
        let called = Arc::new(AtomicBool::new(false));
        let (copy, source, reentrant) = (called.clone(), owner.clone(), admission.clone());
        let ticket = admission
            .admit_host(host, move |a, _| {
                reentrant.statistics().unwrap();
                source.revoke();
                // Approval is a local decision, not permission to ignore source loss.
                a.decide(true).unwrap();
                copy.store(true, Ordering::Release);
                Ok(())
            })
            .unwrap();
        let mut run = Box::pin(hub.serve());
        let mut client = Box::pin(async {
            loop {
                let _ = viewer.drive(Duration::from_millis(1)).await;
            }
        });
        let result = poll_fn(|task| {
            if let Poll::Ready(result) = run.as_mut().poll(task) {
                return Poll::Ready(result);
            }
            assert!(client.as_mut().poll(task).is_pending());
            Poll::Pending
        })
        .await;
        assert!(result.is_err());
        assert!(called.load(Ordering::Acquire));
        assert!(!alive.load(Ordering::Acquire));
        assert!(matches!(ticket.state(), State::Finished(Err(_))));
        drop(client);
        assert!(!viewer.is_complete());
        drop(run);
        assert!(matches!(initial.state(), State::Finished(Err(_))));
        reap(&mut publisher).await;
    });
}

#[test]
fn incoming_host_original_peer_revocation_cannot_be_overridden_by_a_local_allow() {
    let rt = support::runtime();
    rt.block_on(async {
        let (mut publisher, owner, mut first, mut hub, admission, _) =
            Box::pin(fixture(&rt, service::Policy::default(), entropy())).await;
        Box::pin(together(
            &mut hub,
            &mut publisher,
            with_client(&mut first.peer, async {
                let Fresh {
                    host,
                    mut viewer,
                    alive,
                    ..
                } = *Box::pin(fresh(&rt, 14, true, Role::Observe, Duration::from_secs(2))).await;
                let local = Arc::new(Mutex::new(None));
                let copy = local.clone();
                let ticket = admission
                    .admit_host(host, move |a, _| {
                        *copy.lock().unwrap() = Some(a);
                        Ok(())
                    })
                    .unwrap();
                prompt(&mut viewer, &local).await;
                alive.store(false, Ordering::Release);
                local.lock().unwrap().take().unwrap().decide(true).unwrap();
                finished(&mut viewer, &ticket).await;
                assert!(matches!(
                    ticket.state(),
                    State::Finished(Err(service::Error::Session(
                        OpenError::Denied | OpenError::Transport(quic::Error::Unauthorized)
                    )))
                ));
                assert!(!viewer.is_complete());
                assert!(owner.check().is_ok());
            }),
        ))
        .await;
        reap(&mut publisher).await;
    });
}

mod managed;

#[path = "incoming/service.rs"]
mod scoped_service;

mod native;
