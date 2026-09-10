//! Real TLS/UDP startup with private, synthetic identity admission. The public
//! constructor has no fixture mode and still requires installed `LocalAPI` proof.
use super::*;
use asupersync::types::Budget;
#[path = "../../../fr-client/src/startup.rs"]
mod client_startup;
use client_startup::Startup;
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::{LimitOverrides, ProtocolLimits},
};
use std::{
    future::Future,
    pin::pin,
    sync::atomic::AtomicBool,
    task::{Context, Poll, Waker},
};
#[allow(dead_code)]
#[path = "../../../fr-transport/tests/support/mod.rs"]
pub(super) mod support;

fn config(approval: bool) -> Configuration {
    Configuration {
        offer: Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: vec![],
        },
        binding: ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(12),
            remote_session: RemoteSessionId::from_raw(13),
        },
        require_approval: approval,
        startup_timeout: Duration::from_secs(2),
        authority: AuthorityPolicy::plan_defaults(),
        transport: Policy {
            critical_send_records: 1,
            ..Policy::default()
        },
    }
}
struct Viewer {
    startup: Startup,
    quic: QuicRecords,
    routes: ControlRoutes,
    no_ack: bool,
}
impl Viewer {
    fn step(&mut self, cx: &Cx) {
        let current = now(cx).unwrap();
        if self.startup.is_complete() {
            return;
        }
        if let Some((binding, maximum)) = self.startup.binding_to_install(current).unwrap() {
            match self
                .quic
                .bind_control(cx, self.routes, binding.id, maximum, || true)
            {
                Ok(routes) => {
                    self.routes = routes;
                    self.startup.bound(binding.id, current).unwrap();
                }
                Err(quic::Error::Backpressure) => return,
                Err(e) => panic!("client bind failed: {e:?}"),
            }
        }
        let deadline = self.startup.deadline_us();
        if !(self.no_ack && self.routes.inbound.binding != 0)
            && let Some(bytes) = self.startup.pending(current).unwrap()
        {
            match self.quic.send(
                cx,
                Route::Stream(self.routes.outbound),
                bytes,
                deadline,
                || true,
            ) {
                Ok(()) => self.startup.sent(current).unwrap(),
                Err(quic::Error::Backpressure) => return,
                Err(e) => panic!("client send failed: {e:?}"),
            }
        }
        if self.startup.is_complete() {
            return;
        }
        let read = Cell::new(false);
        let mut bytes = [0; 4096];
        let mut len = 0;
        self.quic
            .receive_ready(
                cx,
                || true,
                |_| !read.get(),
                |route, record| {
                    assert_eq!(route, Route::Stream(self.routes.inbound));
                    bytes[..record.len()].copy_from_slice(record);
                    len = record.len();
                    read.set(true);
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
        if read.get() {
            self.startup
                .receive(&bytes[..len], now(cx).unwrap())
                .unwrap();
        }
    }
    async fn drive(&mut self, cx: &Cx) {
        self.step(cx);
        self.quic
            .drive(cx, Duration::from_millis(1), || true)
            .await
            .unwrap();
        self.step(cx);
    }
}
async fn pair(
    cx: &Cx,
    hcx: &Cx,
    configuration: Configuration,
    control: bool,
) -> (Host, Viewer, Arc<AtomicBool>) {
    let (client, server) = support::native_pair(cx, "localhost", quic::ALPN).await;
    let alive = Arc::new(AtomicBool::new(true));
    let peer = Peer::Fixture {
        alive: alive.clone(),
        until: now(hcx).unwrap() + 10_000_000,
        control,
    };
    let (quic, routes) =
        QuicRecords::bootstrap(client.unwrap(), cx, configuration.transport).unwrap();
    let startup = Startup::new(configuration.offer.clone(), now(cx).unwrap(), 2_000_000).unwrap();
    let host = Host::start(hcx.clone(), server.unwrap(), peer, configuration).unwrap();
    (
        host,
        Viewer {
            startup,
            quic,
            routes,
            no_ack: false,
        },
        alive,
    )
}
async fn ready(cx: &Cx, host: &mut Host, viewer: &mut Viewer, approve: bool) {
    for _ in 0..500 {
        let (host_result, ()) = Box::pin(support::both(
            host.drive(Duration::from_millis(1)),
            viewer.drive(cx),
        ))
        .await;
        host_result.unwrap();
        if approve
            && viewer.startup.approval().is_some()
            && let Some(local) = host.approval()
        {
            local.decide(true).unwrap();
        }
        if host.is_complete() && viewer.startup.is_complete() {
            return;
        }
    }
    panic!("startup did not finish: {host:?}");
}
#[test]
fn real_quic_client_host_negotiate_and_session_drop_revokes_observation() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, alive) = pair(&cx, &hcx, config(false), true).await;
        let original = host.transport.as_ref().unwrap().binding();
        assert_eq!(
            host.authority.as_ref().unwrap().phase(),
            fr_core::authority::Phase::Identified
        );
        Box::pin(ready(&cx, &mut host, &mut viewer, false)).await;
        assert!(host.transport.as_ref().unwrap().is_bound_to(&original));
        assert_eq!(host.routes.inbound.binding, 7);
        let mut session = host.finish().unwrap();
        assert_eq!(
            session.selection(),
            &viewer.startup.finish(now(&cx).unwrap()).unwrap().selection
        );
        let observation = session.observation().unwrap();
        observation.check().unwrap();
        session.io().unwrap();
        drop(session);
        assert!(observation.check().is_err());
        assert!(!alive.load(Ordering::Acquire));
    });
}
#[test]
fn actual_approval_notification_never_exposes_observation_before_local_consent() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, _) = pair(&cx, &hcx, config(true), true).await;
        let deadline = host.deadline_us();
        for _ in 0..100 {
            let (host_result, ()) = Box::pin(support::both(
                host.drive(Duration::from_millis(1)),
                viewer.drive(&cx),
            ))
            .await;
            host_result.unwrap();
            if viewer.startup.approval().is_some() {
                break;
            }
        }
        assert!(viewer.startup.approval().is_some());
        assert!(host.observation_until.is_none());
        assert!(!host.is_complete());
        let local = host.approval().unwrap();
        local.decide(true).unwrap();
        assert_eq!(local.decide(true), Err(Error::Order));
        Box::pin(ready(&cx, &mut host, &mut viewer, false)).await;
        assert_eq!(deadline, host.deadline_us());
        let mut session = host.finish().unwrap();
        session.observation().unwrap().check().unwrap();
        assert!(local.decide(true).is_err());
    });
}
#[test]
fn denied_dropped_and_expired_approval_handles_cannot_authorize_equal_id_replacements() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, alive) = pair(&cx, &hcx, config(true), true).await;
        for _ in 0..100 {
            let (host_result, ()) = Box::pin(support::both(
                host.drive(Duration::from_millis(1)),
                viewer.drive(&cx),
            ))
            .await;
            host_result.unwrap();
            if host.approval().is_some() {
                break;
            }
        }
        let old = host.approval().unwrap();
        old.decide(false).unwrap();
        assert_eq!(host.tick(), Err(Error::Denied));
        assert!(!alive.load(Ordering::Acquire));
        assert!(host.finish().is_err());
        let (mut next, _, _) = pair(&cx, &hcx, config(true), true).await;
        assert!(old.decide(true).is_err());
        assert!(next.observation_until.is_none());
        next.close();
        let local = Approval {
            state: Arc::downgrade(&Arc::new(AtomicU8::new(WAITING))),
            cx: cx.clone(),
            deadline: u64::MAX,
        };
        assert_eq!(local.decide(true), Err(Error::Closed));
        let state = Arc::new(AtomicU8::new(WAITING));
        let local = Approval {
            state: Arc::downgrade(&state),
            cx: cx.clone(),
            deadline: now(&cx).unwrap(),
        };
        assert_eq!(local.decide(true), Err(Error::Expired));
    });
}
#[test]
fn silent_native_peer_expires_without_additional_packets() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let mut configuration = config(false);
        configuration.startup_timeout = Duration::from_millis(20);
        let (mut host, _v, alive) = pair(&cx, &hcx, configuration, true).await;
        let mut result = Ok(());
        for _ in 0..20 {
            result = host.drive(Duration::from_millis(10)).await;
            if result.is_err() {
                break;
            }
        }
        assert!(result.is_err());
        assert_eq!(host.phase, Phase::Closed);
        assert!(!alive.load(Ordering::Acquire));
        assert!(host.finish().is_err());
    });
}
#[test]
fn readonly_admission_refuses_control_intent_but_can_open_an_observer() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, _) = pair(&cx, &hcx, config(false), false).await;
        let mut refused = false;
        for _ in 0..100 {
            let (host_result, ()) = Box::pin(support::both(
                host.drive(Duration::from_millis(1)),
                viewer.drive(&cx),
            ))
            .await;
            if host_result.is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused);
        assert!(host.observation_until.is_none());
        let mut configuration = config(false);
        configuration.offer.role = Role::Observe;
        let (mut host, mut viewer, _) = pair(&cx, &hcx, configuration, false).await;
        Box::pin(ready(&cx, &mut host, &mut viewer, false)).await;
        assert_eq!(host.finish().unwrap().selection().role, Role::Observe);
    });
}
#[test]
fn received_selection_before_hello_is_terminal() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, _) = pair(&cx, &hcx, config(false), true).await;
        let mut bytes = [0; 4096];
        let n = negotiation::encode(
            &Message::SelectedConfiguration(config(false).offer.select().unwrap()),
            4096,
            &mut bytes,
        )
        .unwrap();
        viewer
            .quic
            .send(
                &cx,
                Route::Stream(viewer.routes.outbound),
                &bytes[..n],
                now(&cx).unwrap() + 1_000_000,
                || true,
            )
            .unwrap();
        let mut refused = false;
        for _ in 0..100 {
            let (host_result, viewer_result) = Box::pin(support::both(
                host.drive(Duration::from_millis(1)),
                viewer.quic.drive(&cx, Duration::from_millis(1), || true),
            ))
            .await;
            viewer_result.unwrap();
            if host_result.is_err() {
                refused = true;
                break;
            }
        }
        assert!(refused);
        assert!(host.finish().is_err());
    });
}
#[test]
fn no_binding_ack_no_observation_handoff_and_revocation_wins() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, mut viewer, alive) = pair(&cx, &hcx, config(false), true).await;
        viewer.no_ack = true;
        for _ in 0..200 {
            let (host_result, ()) = Box::pin(support::both(
                host.drive(Duration::from_millis(1)),
                viewer.drive(&cx),
            ))
            .await;
            host_result.unwrap();
            if viewer.routes.inbound.binding != 0 {
                break;
            }
        }
        assert_eq!(host.phase, Phase::Ack);
        assert!(!host.is_complete());
        alive.store(false, Ordering::Release);
        assert!(host.tick().is_err());
        assert!(host.finish().is_err());
    });
}
#[test]
fn dropping_a_polled_io_future_closes_native_startup_and_its_admission() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (mut host, _v, alive) = pair(&cx, &hcx, config(false), true).await;
        {
            let mut pending = pin!(host.drive(Duration::from_millis(100)));
            let mut task = Context::from_waker(Waker::noop());
            assert!(matches!(pending.as_mut().poll(&mut task), Poll::Pending));
        }
        assert_eq!(host.phase, Phase::Closed);
        assert!(!alive.load(Ordering::Acquire));
        assert!(host.transport.as_ref().unwrap().is_closed());
        assert!(host.finish().is_err());
    });
}
#[test]
fn tiny_reply_budget_and_unqualified_configuration_fail_without_capture_or_input() {
    let runtime = support::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    for configuration in [
        Configuration {
            startup_timeout: Duration::ZERO,
            ..config(false)
        },
        Configuration {
            startup_timeout: Duration::from_secs(61),
            ..config(false)
        },
        Configuration {
            binding: ControlBinding {
                id: 0,
                ..config(false).binding
            },
            ..config(false)
        },
    ] {
        assert!(configuration.validate().is_err());
    }
    runtime.block_on(async {
        let mut configuration = config(false);
        configuration.offer.limits = ProtocolLimits::with_overrides(LimitOverrides {
            max_control_message_bytes: Some(128),
            ..Default::default()
        })
        .unwrap();
        let (mut host, mut viewer, _) = pair(&cx, &hcx, configuration, true).await;
        let mut failed = false;
        for _ in 0..100 {
            let (host_result, ()) = Box::pin(support::both(
                host.drive(Duration::from_millis(1)),
                viewer.drive(&cx),
            ))
            .await;
            if host_result.is_err() {
                failed = true;
                break;
            }
        }
        assert!(failed);
        assert!(!host.is_complete());
        assert!(host.finish().is_err());
    });
}
