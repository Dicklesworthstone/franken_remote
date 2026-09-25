#![cfg(target_os = "linux")]
#![recursion_limit = "256"]
//! Real native TLS/UDP/Host, policy store and bounded subprocess supervision.
//! Privileged interface/firewall evidence and `LocalAPI` metadata are SYNTHETIC.
//! Run via `scripts/test_linux_serial_lifecycle.sh` in its fresh mount/net namespace.
//! These tests do NOT establish kernel ingress or installed-tailnet qualification.
#[allow(dead_code)]
#[path = "native_host_accept/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../../fr-transport/tests/support/mod.rs"]
mod network;
use asupersync::{
    cx::Cx,
    runtime::RuntimeHandle,
    time::{sleep, timeout},
    types::{Budget, CancelKind},
};
use fr_tailnet::{LocalApi, ingress};
use fr_transport::native_accept::{self, Listener};
use frd::{
    host_policy::{self, Change, Store, live},
    media::ObservationControl,
    native_connection::host::{
        Error as HostError, IngressCheck, LinuxError, Request, Server, serial,
    },
    session_startup::{Host, HostSession, Viewer, ViewerSession},
};
use std::{
    fs,
    future::{Future, pending, poll_fn},
    net::{SocketAddr, UdpSocket},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    task::Poll,
    time::Duration,
};

struct Tools(PathBuf);
impl Tools {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        assert_eq!(
            fs::read_to_string("/run/fr-synthetic-ingress").unwrap(),
            "synthetic-only\n"
        );
        let dir = PathBuf::from(format!(
            "/run/fr-serial-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["nft", "ip"] {
            let file = dir.join(name);
            fs::write(&file, include_str!("native_host_linux_serial/tools.py")).unwrap();
            fs::set_permissions(file, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(dir)
    }
    fn configuration(&self) -> ingress::Configuration {
        ingress::Configuration::new(address(), "fr-fixture")
            .unwrap()
            .executables(&self.0.join("nft"), &self.0.join("ip"))
            .unwrap()
    }
    fn state(&self) -> serde_json::Value {
        serde_json::from_slice(&fs::read(self.0.join("state.json")).unwrap()).unwrap()
    }
}
fn address() -> SocketAddr {
    "100.64.0.1:4791".parse().unwrap()
}
fn run(body: impl AsyncFnOnce(Cx, Cx, Cx, RuntimeHandle)) {
    let runtime = network::runtime();
    let broker = runtime.request_cx_with_budget(Budget::INFINITE);
    let supervisor = runtime.request_cx_with_budget(Budget::INFINITE);
    let client = runtime.request_cx_with_budget(Budget::INFINITE);
    let handle = runtime.handle();
    runtime.block_on(async {
        timeout(
            client.now(),
            Duration::from_secs(12),
            body(broker, supervisor, client, handle),
        )
        .await
        .unwrap();
    });
}
async fn until(cx: &Cx, ready: impl Fn() -> bool) {
    while !ready() {
        sleep(cx.now(), Duration::from_millis(5)).await;
    }
}
async fn negotiate(cx: Cx) -> ViewerSession {
    let native = fixture::client(&cx, address()).await;
    let mut viewer = Viewer::new(
        cx,
        native,
        fixture::offer(),
        fr_transport::quic::Policy::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    while !viewer.is_complete() {
        viewer.drive(Duration::from_millis(5)).await.unwrap();
    }
    viewer.finish().unwrap()
}
#[derive(Default)]
struct App {
    requests: Arc<AtomicU64>,
    completions: Arc<AtomicU64>,
    observed: Arc<Mutex<Vec<ObservationControl>>>,
    approvals: Arc<AtomicU64>,
    approval_override: Option<bool>,
    outcomes: Vec<Result<u128, HostError>>,
    retained: Option<HostSession>,
    retain: bool,
    park_first: bool,
    panic_request: bool,
    tamper: Option<PathBuf>,
    stop_after: u64,
}
impl serial::Application for App {
    type Output = u128;
    fn request(&mut self, attempt: u64) -> Result<Request, serial::Error> {
        assert!(!self.panic_request, "intentional local request panic");
        self.requests.store(attempt, Ordering::Release);
        let mut request = fixture::request();
        request.connection_id =
            asupersync::net::quic_core::ConnectionId::new(&u128::from(attempt).to_be_bytes())
                .unwrap();
        request.session.binding.remote_session =
            fr_core::ids::RemoteSessionId::from_raw(u128::from(attempt) + 10);
        if let Some(required) = self.approval_override {
            request.session.require_approval = required;
        }
        Ok(request)
    }
    async fn serve(&mut self, host: Host) -> u128 {
        let mut session = host
            .open(Duration::from_millis(5), |approval, _| {
                self.approvals.fetch_add(1, Ordering::AcqRel);
                approval.decide(true).unwrap();
                Ok(())
            })
            .await
            .unwrap();
        let id = session.binding().remote_session.as_raw();
        self.observed
            .lock()
            .unwrap()
            .push(session.observation().unwrap());
        if self.park_first && self.requests.load(Ordering::Acquire) == 1 {
            pending::<()>().await;
        }
        if self.retain {
            self.retained = Some(session);
        }
        id
    }
    fn completed(
        &mut self,
        stats: serial::Statistics,
        outcome: Result<u128, HostError>,
    ) -> Result<serial::Action, serial::Error> {
        self.outcomes.push(outcome);
        self.completions.store(stats.attempts, Ordering::Release);
        if let Some(path) = &self.tamper {
            fs::write(path, b"synthetic rule removed").unwrap();
        }
        Ok(if stats.attempts >= self.stop_after {
            serial::Action::Stop
        } else {
            serial::Action::Continue
        })
    }
}

#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn successive_sessions_share_one_boundary_past_its_initial_expiry() {
    run(async |broker, supervisor, client, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity.clone())
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut app = App {
            stop_after: 2,
            ..App::default()
        };
        let completed = app.completions.clone();
        let serving = server.serve_serial(
            supervisor.clone(),
            handle.clone(),
            serial::Policy::default(),
            &mut app,
        );
        let clients = async {
            let first =
                negotiate(handle.try_request_cx_with_budget(Budget::INFINITE).unwrap()).await;
            until(&client, || completed.load(Ordering::Acquire) == 1).await;
            drop(first);
            sleep(client.now(), Duration::from_millis(3300)).await;
            negotiate(handle.try_request_cx_with_budget(Budget::INFINITE).unwrap()).await
        };
        let (result, second) = Box::pin(network::both(serving, clients)).await;
        assert_eq!(
            result.unwrap(),
            serial::Statistics {
                attempts: 2,
                admitted: 2,
                refused: 0
            }
        );
        assert_eq!(app.outcomes, [Ok(11), Ok(12)]);
        assert!(
            app.observed
                .lock()
                .unwrap()
                .iter()
                .all(|o| o.check().is_err())
        );
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(identity.status(&broker).is_ok());
        assert_eq!(tools.state()["created"], 1);
        assert!(tools.state()["reads"].as_u64().unwrap() >= 5);
        assert_eq!(tools.state()["deleted"], 0);
        drop(second);
        server.stop(&broker).await.unwrap();
        server.stop(&broker).await.unwrap();
        assert_eq!(tools.state()["deleted"], 1);
        assert!(UdpSocket::bind(address()).is_ok());
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn idle_attempts_rebind_without_reinstalling_or_extending_their_deadlines() {
    run(async |broker, supervisor, _, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity)
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration {
                    initial_timeout: Duration::from_millis(40),
                    ..native_accept::Configuration::default()
                },
            )
            .await
            .unwrap();
        let mut app = App {
            stop_after: 3,
            ..App::default()
        };
        let stats = server
            .serve_serial(
                supervisor,
                handle,
                serial::Policy {
                    cooldown: Duration::from_millis(600),
                    ..serial::Policy::default()
                },
                &mut app,
            )
            .await
            .unwrap();
        assert_eq!(
            stats,
            serial::Statistics {
                attempts: 3,
                admitted: 0,
                refused: 3
            }
        );
        assert!(
            app.outcomes
                .iter()
                .all(|o| *o == Err(HostError::Accept(native_accept::Error::InitialTimeout)))
        );
        assert_eq!(api.whois.load(Ordering::Acquire), 0);
        assert_eq!(tools.state()["created"], 1);
        assert!(tools.state()["reads"].as_u64().unwrap() >= 3);
        server.stop(&broker).await.unwrap();
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn lost_rule_during_cooldown_prevents_any_new_bind_or_admission() {
    run(async |broker, supervisor, _, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let mut server = Server::new(api.client.clone(), api.identity(&broker).await)
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration {
                    initial_timeout: Duration::from_millis(40),
                    ..native_accept::Configuration::default()
                },
            )
            .await
            .unwrap();
        let mut app = App {
            stop_after: 99,
            tamper: Some(tools.0.join("tamper")),
            ..App::default()
        };
        let result = server
            .serve_serial(
                supervisor.clone(),
                handle,
                serial::Policy {
                    cooldown: Duration::from_millis(900),
                    ..serial::Policy::default()
                },
                &mut app,
            )
            .await;
        assert_eq!(
            result,
            Err(LinuxError::Ingress(ingress::Error::FirewallMismatch))
        );
        assert_eq!(app.requests.load(Ordering::Acquire), 1);
        assert_eq!(app.outcomes.len(), 1);
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(UdpSocket::bind(address()).is_ok());
        server.stop(&broker).await.unwrap();
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn retained_original_transport_blocks_rebind_and_firewall_removal() {
    run(async |broker, supervisor, client, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let mut server = Server::new(api.client.clone(), api.identity(&broker).await)
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut app = App {
            stop_after: 2,
            retain: true,
            ..App::default()
        };
        let (result, peer) = Box::pin(network::both(
            server.serve_serial(
                supervisor,
                handle,
                serial::Policy {
                    retirement_timeout: Duration::from_millis(80),
                    ..serial::Policy::default()
                },
                &mut app,
            ),
            negotiate(client),
        ))
        .await;
        assert_eq!(
            result,
            Err(LinuxError::Serial(serial::Error::RetainedTransport))
        );
        assert_eq!(app.outcomes, [Ok(11)]);
        assert_eq!(app.requests.load(Ordering::Acquire), 1);
        assert_eq!(server.stop(&broker).await, Err(ingress::Error::InUse));
        assert_eq!(tools.state()["deleted"], 0);
        drop(app.retained.take());
        drop(peer);
        server.stop(&broker).await.unwrap();
        assert_eq!(tools.state()["deleted"], 1);
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn unpolled_service_and_caught_request_panic_fence_only_the_supervisor() {
    run(async |broker, supervisor, client, handle| {
        for panic_request in [false, true] {
            let tools = Tools::new();
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone())
                .bind_linux(
                    &broker,
                    tools.configuration(),
                    native_accept::Configuration::default(),
                )
                .await
                .unwrap();
            let context = if panic_request {
                client.clone()
            } else {
                supervisor.clone()
            };
            let mut app = App {
                panic_request,
                ..App::default()
            };
            let mut work = Box::pin(server.serve_serial(
                context.clone(),
                handle.clone(),
                serial::Policy::default(),
                &mut app,
            ));
            if panic_request {
                poll_fn(|task| {
                    assert!(
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work
                            .as_mut()
                            .poll(task)))
                        .is_err()
                    );
                    assert!(context.is_cancel_requested());
                    Poll::Ready(())
                })
                .await;
            }
            drop(work);
            assert!(context.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert_eq!(app.requests.load(Ordering::Acquire), 0);
            assert!(UdpSocket::bind(address()).is_ok());
            server.stop(&broker).await.unwrap();
            let mut spent = App::default();
            assert_eq!(
                server
                    .serve_serial(
                        handle.try_request_cx_with_budget(Budget::INFINITE).unwrap(),
                        handle.clone(),
                        serial::Policy::default(),
                        &mut spent
                    )
                    .await,
                Err(LinuxError::Spent)
            );
        }
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn changed_live_policy_retires_old_peer_and_new_peer_requires_approval() {
    policy_handover(false);
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn stopped_live_monitor_is_terminal_without_rebinding() {
    policy_handover(true);
}
fn policy_handover(stop: bool) {
    run(async move |broker, supervisor, client, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let path = tools.0.join("policy.json");
        Store::new(&path)
            .unwrap()
            .update(Change::Approval(host_policy::Approval::None))
            .unwrap();
        let watch = live::Watch::start(&broker, Store::new(&path).unwrap()).unwrap();
        let policy = watch.handle();
        until(&broker, || {
            matches!(policy.status(), live::Status::Active(_))
        })
        .await;
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity.clone())
            .with_live_policy(policy)
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut app = App {
            stop_after: 2,
            park_first: true,
            approval_override: Some(false),
            ..App::default()
        };
        let observed = app.observed.clone();
        let requests = app.requests.clone();
        let completed = app.completions.clone();
        let action = async {
            let peer = negotiate(client.clone()).await;
            until(&client, || !observed.lock().unwrap().is_empty()).await;
            assert_eq!(observed.lock().unwrap().len(), 1);
            if stop {
                watch.stop();
                return peer;
            }
            Store::new(&path)
                .unwrap()
                .update(Change::Approval(host_policy::Approval::Local))
                .unwrap();
            until(&broker, || completed.load(Ordering::Acquire) == 1).await;
            assert!(observed.lock().unwrap()[0].check().is_err());
            drop(peer);
            until(&broker, || requests.load(Ordering::Acquire) == 2).await;
            negotiate(handle.try_request_cx_with_budget(Budget::INFINITE).unwrap()).await
        };
        let (result, peer) = Box::pin(network::both(
            server.serve_serial(
                supervisor.clone(),
                handle.clone(),
                serial::Policy::default(),
                &mut app,
            ),
            action,
        ))
        .await;
        if stop {
            assert_eq!(
                result,
                Err(LinuxError::Serial(serial::Error::Host(HostError::Policy(
                    live::Error::Closed
                ))))
            );
            assert_eq!(app.outcomes, [Err(HostError::Policy(live::Error::Closed))]);
            assert_eq!(requests.load(Ordering::Acquire), 1);
            assert_eq!(app.approvals.load(Ordering::Acquire), 0);
        } else {
            assert_eq!(
                result,
                Ok(serial::Statistics {
                    attempts: 2,
                    admitted: 1,
                    refused: 1
                })
            );
            assert_eq!(
                app.outcomes,
                [Err(HostError::Policy(live::Error::Changed)), Ok(12)]
            );
            assert_eq!(requests.load(Ordering::Acquire), 2);
            assert_eq!(app.approvals.load(Ordering::Acquire), 1);
        }
        assert_eq!(tools.state()["created"], 1);
        assert!(observed.lock().unwrap()[0].check().is_err());
        assert!(supervisor.is_cancel_requested());
        assert!(!broker.is_cancel_requested());
        assert!(identity.status(&broker).is_ok());
        drop(peer);
        server.stop(&broker).await.unwrap();
        retire_watch(&broker, watch).await;
    });
}
#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn cancellation_fences_parked_peer_before_the_failed_future_is_dropped() {
    run(async |broker, supervisor, client, handle| {
        let tools = Tools::new();
        let api = fixture::Api::new();
        let mut server = Server::new(api.client.clone(), api.identity(&broker).await)
            .bind_linux(
                &broker,
                tools.configuration(),
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        let mut app = App {
            park_first: true,
            stop_after: 99,
            ..App::default()
        };
        let observed = app.observed.clone();
        let mut work = Box::pin(server.serve_serial(
            supervisor.clone(),
            handle,
            serial::Policy::default(),
            &mut app,
        ));
        let revoke = async {
            let peer = negotiate(client.clone()).await;
            until(&client, || !observed.lock().unwrap().is_empty()).await;
            supervisor.cancel_fast(CancelKind::User);
            peer
        };
        let ((), peer) = Box::pin(network::both(
            poll_fn(|task| {
                let Poll::Ready(result) = work.as_mut().poll(task) else {
                    return Poll::Pending;
                };
                assert_eq!(result, Err(LinuxError::Serial(serial::Error::Cancelled)));
                assert!(observed.lock().unwrap()[0].check().is_err());
                assert!(!broker.is_cancel_requested());
                Poll::Ready(())
            }),
            revoke,
        ))
        .await;
        drop(work);
        drop(peer);
        server.stop(&broker).await.unwrap();
        assert_eq!(app.outcomes, []);
    });
}

async fn retire_watch(cx: &Cx, mut watch: live::Watch) {
    watch.stop();
    loop {
        if let Some(result) = watch.try_finish() {
            result.unwrap();
            return;
        }
        sleep(cx.now(), Duration::from_millis(5)).await;
    }
}

#[path = "native_host_linux_serial/desktop.rs"]
mod desktop;

#[path = "native_host_linux_serial/persistent_desktop.rs"]
mod persistent_desktop;

#[path = "native_host_linux_serial/host_run.rs"]
mod host_run;

#[path = "native_host_linux_serial/shipped_client.rs"]
mod shipped_client;

#[path = "native_host_linux_serial/real_media.rs"]
mod real_media;
