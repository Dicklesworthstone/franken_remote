#![cfg(target_os = "linux")]
//! Real TLS/UDP and authenticated Unix HTTP. Tailnet metadata, CA and ingress
//! lifetime are fixtures. Run explicitly in an isolated user/network namespace,
//! with 100.64.0.1/32 and 100.64.0.2/32 on lo; NOT kernel/Tailscale qualification.
#[allow(dead_code)]
#[path = "native_host_accept/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../../fr-transport/tests/support/mod.rs"]
mod network;
use asupersync::{cx::Cx, runtime::RuntimeHandle, types::Budget};
use fr_tailnet::LocalApi;
use fr_transport::native_accept::{self, Listener};
use frd::{
    native_connection::host::{Error as HostError, IngressCheck, Request, Server, serial::*},
    session_startup::{Host, Viewer},
};
use std::{
    future::Future,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
fn run(f: impl FnOnce(Cx, Cx, RuntimeHandle) -> std::pin::Pin<Box<dyn Future<Output = ()>>>) {
    let runtime = network::runtime();
    let handle = runtime.handle();
    let broker = handle.try_request_cx_with_budget(Budget::INFINITE).unwrap();
    let supervisor = handle.try_request_cx_with_budget(Budget::INFINITE).unwrap();
    runtime.block_on(async {
        asupersync::time::timeout(
            broker.now(),
            Duration::from_secs(15),
            f(broker.clone(), supervisor, handle),
        )
        .await
        .unwrap();
    });
}
fn policy() -> Policy {
    Policy {
        cooldown: Duration::from_millis(100),
        retirement_timeout: Duration::from_millis(200),
    }
}
async fn until(cx: &Cx, ready: impl Fn() -> bool) {
    while !ready() {
        asupersync::time::sleep(cx.now(), Duration::from_millis(2)).await;
    }
}
struct App {
    requests: Arc<AtomicU64>,
    completed: u64,
    served: u64,
    stop: u64,
    reuse: bool,
    retain: Option<Arc<Mutex<Option<Host>>>>,
    refused: Vec<HostError>,
    collect: bool,
    panic_after_store: bool,
    request_times: Vec<std::time::Instant>,
    completion_times: Vec<std::time::Instant>,
}
impl App {
    fn new(stop: u64) -> Self {
        Self {
            requests: Arc::new(AtomicU64::new(0)),
            completed: 0,
            served: 0,
            stop,
            reuse: false,
            retain: None,
            refused: vec![],
            collect: false,
            panic_after_store: false,
            request_times: vec![],
            completion_times: vec![],
        }
    }
}
impl Application for App {
    type Output = u64;
    fn request(&mut self, attempt: u64) -> Result<Request, Error> {
        self.request_times.push(std::time::Instant::now());
        self.requests.store(attempt, Ordering::Release);
        let n = if self.reuse { 1 } else { attempt };
        let mut request = fixture::request();
        request.connection_id =
            asupersync::net::quic_core::ConnectionId::new(&(100 + u128::from(n)).to_be_bytes())
                .unwrap();
        request.session.binding.remote_session =
            fr_core::ids::RemoteSessionId::from_raw(1000 + u128::from(n));
        Ok(request)
    }
    async fn serve(&mut self, host: Host) -> u64 {
        self.served += 1;
        if let Some(retain) = &self.retain {
            *retain.lock().unwrap() = Some(host);
            assert!(
                !self.panic_after_store,
                "application panic after ownership escaped"
            );
        } else {
            let session = host
                .open(Duration::from_millis(5), |approval, _| {
                    approval.decide(true).map_err(|_| ())
                })
                .await
                .unwrap();
            drop(session);
        }
        self.served
    }
    fn completed(
        &mut self,
        stats: Statistics,
        outcome: Result<u64, HostError>,
    ) -> Result<Action, Error> {
        self.completion_times.push(std::time::Instant::now());
        if self.collect
            && let Some(retain) = self.retain.take()
        {
            drop(retain.lock().unwrap().take());
        }
        self.completed += 1;
        assert_eq!(self.completed, stats.attempts);
        match outcome {
            Ok(n) => assert_eq!(n, self.served),
            Err(e) => self.refused.push(e),
        }
        Ok(if self.completed == self.stop {
            Action::Stop
        } else {
            Action::Continue
        })
    }
}
async fn viewer(cx: &Cx, address: SocketAddr) {
    let native = fixture::client(cx, address).await;
    let mut viewer = Viewer::new(
        cx.clone(),
        native,
        fixture::offer(),
        fr_transport::quic::Policy::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    while !viewer.is_complete() {
        viewer.drive(Duration::from_millis(5)).await.unwrap();
    }
    drop(viewer.finish().unwrap());
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn two_actual_sessions_reuse_destination_without_reusing_identity_or_scope() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let mut app = App::new(2);
            let requested = app.requests.clone();
            let clients = async {
                for n in 1..=2 {
                    until(&broker, || requested.load(Ordering::Acquire) >= n).await;
                    asupersync::time::sleep(broker.now(), Duration::from_millis(10)).await;
                    let client = runtime
                        .try_request_cx_with_budget(Budget::INFINITE)
                        .unwrap();
                    viewer(&client, address).await;
                }
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime.clone(),
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, clients)).await;
            assert_eq!(
                result,
                Ok(Statistics {
                    attempts: 2,
                    admitted: 2,
                    refused: 0
                })
            );
            assert_eq!((app.completed, app.served), (2, 2));
            assert!(
                app.request_times[1].duration_since(app.completion_times[0]) >= policy().cooldown
            );
            assert!(api.whois.load(Ordering::Acquire) >= 2);
            assert!(supervisor.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn idle_deadline_reopens_after_cooldown_then_accepts_real_tls() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity);
            let config = native_accept::Configuration {
                initial_timeout: Duration::from_millis(120),
                ..Default::default()
            };
            let listener = Listener::bind(&broker, "100.64.0.1:0".parse().unwrap(), config)
                .await
                .unwrap();
            let address = listener.local_addr();
            let mut app = App::new(2);
            let requested = app.requests.clone();
            let client = async {
                until(&broker, || requested.load(Ordering::Acquire) == 2).await;
                asupersync::time::sleep(broker.now(), Duration::from_millis(10)).await;
                let client = runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .unwrap();
                viewer(&client, address).await;
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor,
                runtime.clone(),
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, client)).await;
            assert_eq!(
                result,
                Ok(Statistics {
                    attempts: 2,
                    admitted: 1,
                    refused: 1
                })
            );
            assert_eq!(
                app.refused,
                [HostError::Accept(native_accept::Error::InitialTimeout)]
            );
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn retained_original_transport_blocks_rebinding_and_reports_cleanup_failure() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let retained = Arc::new(Mutex::new(None));
            let mut app = App::new(2);
            app.retain = Some(retained.clone());
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime,
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, client) =
                Box::pin(network::both(service, fixture::client(&broker, address))).await;
            assert_eq!(result, Err(Error::RetainedTransport));
            assert_eq!(app.requests.load(Ordering::Acquire), 1);
            assert_eq!(app.completed, 1);
            assert!(supervisor.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert!(UdpSocket::bind(address).is_err());
            drop(retained.lock().unwrap().take());
            drop(client);
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn unpolled_drop_closes_socket_without_request_or_broker_cancellation() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let mut app = App::new(2);
            drop(server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime,
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            ));
            assert_eq!(app.requests.load(Ordering::Acquire), 0);
            assert!(supervisor.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn stopped_ingress_wakes_idle_service_and_never_starts_a_replacement() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let mut app = App::new(2);
            let requested = app.requests.clone();
            let live = Arc::new(AtomicBool::new(true));
            let revoke = async {
                until(&broker, || requested.load(Ordering::Acquire) == 1).await;
                live.store(false, Ordering::Release);
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime,
                listener,
                policy(),
                fixture::boundary(address, live.clone()),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, revoke)).await;
            assert_eq!(result, Err(Error::Host(HostError::IngressUnavailable)));
            assert_eq!(app.requests.load(Ordering::Acquire), 1);
            assert_eq!(app.completed, 0);
            assert!(identity.status(&broker).is_ok());
            assert!(!broker.is_cancel_requested());
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn duplicate_peer_ids_refuse_before_a_second_socket_or_application() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity);
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let mut app = App::new(2);
            app.reuse = true;
            let client = runtime
                .try_request_cx_with_budget(Budget::INFINITE)
                .unwrap();
            let service = server.serve_serial_on_protected_listener(
                supervisor,
                runtime,
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, viewer(&client, address))).await;
            assert_eq!(result, Err(Error::IdentityReuse));
            assert_eq!((app.completed, app.served), (1, 1));
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn unauthorized_user_is_refused_then_new_authorized_peer_gets_its_own_session() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            *api.mode.lock().unwrap() = fixture::Mode::OtherUser;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let mut app = App::new(2);
            let requests = app.requests.clone();
            let clients = async {
                let refused = fixture::client(&broker, address).await;
                until(&broker, || requests.load(Ordering::Acquire) == 2).await;
                *api.mode.lock().unwrap() = fixture::Mode::Allowed;
                drop(refused);
                asupersync::time::sleep(broker.now(), Duration::from_millis(10)).await;
                let client = runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .unwrap();
                viewer(&client, address).await;
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor,
                runtime.clone(),
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, clients)).await;
            assert_eq!(
                result,
                Ok(Statistics {
                    attempts: 2,
                    admitted: 1,
                    refused: 1
                })
            );
            assert_eq!(
                app.refused,
                [HostError::Tailnet(fr_tailnet::Error::ScopeDenied)]
            );
            assert_eq!(app.served, 1);
            assert!(identity.status(&broker).is_ok());
            assert!(
                app.request_times[1].duration_since(app.completion_times[0]) >= policy().cooldown
            );
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn changed_host_identity_is_terminal_even_when_local_callback_requests_continue() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            *api.mode.lock().unwrap() = fixture::Mode::ChangeAfterWhoIs;
            let mut server = Server::new(api.client.clone(), identity);
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let mut app = App::new(2);
            let service = server.serve_serial_on_protected_listener(
                supervisor,
                runtime,
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, client) =
                Box::pin(network::both(service, fixture::client(&broker, address))).await;
            assert_eq!(
                result,
                Err(Error::Host(HostError::Tailnet(
                    fr_tailnet::Error::IdentityChanged
                )))
            );
            assert_eq!((app.completed, app.served), (1, 0));
            assert_eq!(app.requests.load(Ordering::Acquire), 1);
            assert_eq!(
                app.refused,
                [HostError::Tailnet(fr_tailnet::Error::IdentityChanged)]
            );
            drop(client);
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn completion_callback_can_release_original_transport_before_the_next_peer() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity);
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let retained = Arc::new(Mutex::new(None));
            let mut app = App::new(2);
            app.retain = Some(retained.clone());
            app.collect = true;
            let requests = app.requests.clone();
            let clients = async {
                let first = fixture::client(&broker, address).await;
                until(&broker, || requests.load(Ordering::Acquire) == 2).await;
                assert!(retained.lock().unwrap().is_none());
                drop(first);
                asupersync::time::sleep(broker.now(), Duration::from_millis(10)).await;
                let client = runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .unwrap();
                viewer(&client, address).await;
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor,
                runtime.clone(),
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, clients)).await;
            assert_eq!(
                result,
                Ok(Statistics {
                    attempts: 2,
                    admitted: 2,
                    refused: 0
                })
            );
            assert_eq!((app.completed, app.served), (2, 2));
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn caught_application_panic_retires_peer_while_failed_service_is_retained() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let retained = Arc::new(Mutex::new(None));
            let mut app = App::new(2);
            app.retain = Some(retained.clone());
            app.panic_after_store = true;
            let mut service = Box::pin(server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime,
                listener,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            ));
            let caught = std::future::poll_fn(|task| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    service.as_mut().poll(task)
                })) {
                    Err(_) => {
                        assert!(supervisor.is_cancel_requested());
                        std::task::Poll::Ready(())
                    }
                    Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
                    Ok(std::task::Poll::Ready(value)) => panic!("unexpected completion {value:?}"),
                }
            });
            let ((), client) =
                Box::pin(network::both(caught, fixture::client(&broker, address))).await;
            // The service is still retained: cancellation must not depend on Drop.
            let host = retained.lock().unwrap().take().unwrap();
            assert!(
                host.open(Duration::from_millis(5), |_, _| panic!("no late consent"))
                    .await
                    .is_err()
            );
            assert!(identity.status(&broker).is_ok());
            assert!(!broker.is_cancel_requested());
            drop(service);
            drop(client);
            assert_eq!(app.completed, 0);
            assert_eq!(app.served, 1);
            assert!(UdpSocket::bind(address).is_ok());
        })
    });
}

#[path = "native_host_serial/live_policy.rs"]
mod live_policy;
