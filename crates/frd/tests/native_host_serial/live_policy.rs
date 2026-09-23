//! Real policy-store handover on the original serial TLS/UDP listener. The
//! existing private LocalAPI/CA/ingress fixtures are not live-tailnet evidence.
use super::*;
use frd::host_policy::{
    Approval, Change, Store,
    live::{self, Handle, Status, Watch},
};
use std::future::pending;

async fn watched(cx: &Cx) -> (Store, Watch, Handle) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = fixture::pki().join(format!(
        "serial-policy-{}",
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let store = Store::new(&path).unwrap();
    let watch = Watch::start(cx, Store::new(&path).unwrap()).unwrap();
    let handle = watch.handle();
    until(cx, || handle.status() != Status::Opening).await;
    assert!(matches!(handle.status(), Status::Active(_)));
    (store, watch, handle)
}
async fn retire(cx: &Cx, mut watch: Watch) {
    watch.stop();
    while watch.try_finish().is_none() {
        asupersync::time::sleep(cx.now(), Duration::from_millis(2)).await;
    }
}
struct PolicyApp {
    app: App,
    approvals: Arc<AtomicU64>,
    holding: Arc<AtomicBool>,
}
impl PolicyApp {
    fn new() -> Self {
        Self {
            app: App::new(2),
            approvals: Arc::new(AtomicU64::new(0)),
            holding: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl Application for PolicyApp {
    type Output = u64;
    fn request(&mut self, n: u64) -> Result<Request, Error> {
        let mut request = self.app.request(n)?;
        // Deliberately contrary to the replacement policy. The new epoch, not
        // this static caller flag, must require actual local approval.
        request.session.require_approval = false;
        Ok(request)
    }
    async fn serve(&mut self, host: Host) -> u64 {
        self.app.served += 1;
        if self.app.served == 1
            && let Some(retain) = &self.app.retain
        {
            *retain.lock().unwrap() = Some(host);
            self.holding.store(true, Ordering::Release);
            return pending().await;
        }
        let approvals = self.approvals.clone();
        let session = host
            .open(Duration::from_millis(5), move |approval, _| {
                approvals.fetch_add(1, Ordering::AcqRel);
                approval.decide(true).map_err(|_| ())
            })
            .await
            .unwrap();
        drop(session);
        self.app.served
    }
    fn completed(
        &mut self,
        stats: Statistics,
        outcome: Result<u64, HostError>,
    ) -> Result<Action, Error> {
        self.app.completed(stats, outcome)
    }
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn changed_policy_retires_pending_acceptance_then_new_peer_uses_new_approval() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let (store, watch, handle) = watched(&broker).await;
            let mut server =
                Server::new(api.client.clone(), identity.clone()).with_live_policy(handle.clone());
            let socket = fixture::listener(&broker).await;
            let address = socket.local_addr();
            let mut app = PolicyApp::new();
            let requests = app.app.requests.clone();
            let clients = async {
                until(&broker, || requests.load(Ordering::Acquire) == 1).await;
                let revision = store
                    .update(Change::Approval(Approval::Local))
                    .unwrap()
                    .policy
                    .revision;
                until(
                    &broker,
                    || matches!(handle.status(), Status::Active(p) if p.revision == revision),
                )
                .await;
                until(&broker, || requests.load(Ordering::Acquire) == 2).await;
                let client = runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .unwrap();
                viewer(&client, address).await;
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime.clone(),
                socket,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let service = async {
                let result = service.await;
                assert!(result.is_ok(), "policy handover ended hosting: {result:?}");
                result
            };
            let (result, ()) = Box::pin(network::both(service, clients)).await;
            assert_eq!(
                result,
                Ok(Statistics {
                    attempts: 2,
                    admitted: 1,
                    refused: 1
                })
            );
            assert_eq!(app.app.refused, [HostError::Policy(live::Error::Changed)]);
            assert_eq!(app.approvals.load(Ordering::Acquire), 1);
            assert!(supervisor.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert!(UdpSocket::bind(address).is_ok());
            retire(&broker, watch).await;
        })
    });
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn changed_policy_waits_for_old_transport_retirement_before_new_peer() {
    changed_active(false);
}
#[test]
#[ignore = "explicit isolated user/network namespace"]
fn changed_policy_cannot_overlap_a_retained_old_transport() {
    changed_active(true);
}
fn changed_active(keep: bool) {
    run(move |broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let (store, watch, handle) = watched(&broker).await;
            let mut server =
                Server::new(api.client.clone(), identity.clone()).with_live_policy(handle);
            let socket = fixture::listener(&broker).await;
            let address = socket.local_addr();
            let held = Arc::new(Mutex::new(None));
            let mut app = PolicyApp::new();
            app.app.retain = Some(held.clone());
            app.app.collect = !keep;
            let holding = app.holding.clone();
            let requests = app.app.requests.clone();
            let clients = async {
                let first = runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .unwrap();
                let native = fixture::client(&first, address).await;
                until(&broker, || holding.load(Ordering::Acquire)).await;
                store.update(Change::Approval(Approval::Local)).unwrap();
                if !keep {
                    until(&broker, || requests.load(Ordering::Acquire) == 2).await;
                    assert!(held.lock().unwrap().is_none());
                    let next = runtime
                        .try_request_cx_with_budget(Budget::INFINITE)
                        .unwrap();
                    viewer(&next, address).await;
                }
                drop(native);
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime.clone(),
                socket,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, clients)).await;
            assert_eq!(app.app.refused, [HostError::Policy(live::Error::Changed)]);
            if keep {
                assert_eq!(result, Err(Error::RetainedTransport));
                assert_eq!(
                    app.app.completed, 1,
                    "preserve the original refusal receipt"
                );
                assert_eq!(requests.load(Ordering::Acquire), 1);
                assert!(UdpSocket::bind(address).is_err());
                drop(held.lock().unwrap().take());
            } else {
                assert_eq!(
                    result,
                    Ok(Statistics {
                        attempts: 2,
                        admitted: 1,
                        refused: 1
                    })
                );
                assert_eq!(app.approvals.load(Ordering::Acquire), 1);
            }
            assert!(UdpSocket::bind(address).is_ok());
            assert!(identity.status(&broker).is_ok());
            retire(&broker, watch).await;
        })
    });
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn stopped_policy_monitor_refuses_before_request_callback_or_rebinding() {
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let (_, watch, handle) = watched(&broker).await;
            let mut server =
                Server::new(api.client.clone(), identity.clone()).with_live_policy(handle);
            let socket = fixture::listener(&broker).await;
            let address = socket.local_addr();
            watch.stop();
            let mut app = App::new(2);
            let result = server
                .serve_serial_on_protected_listener(
                    supervisor.clone(),
                    runtime,
                    socket,
                    policy(),
                    fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                    &mut app,
                )
                .await;
            assert_eq!(
                result,
                Err(Error::Host(HostError::Policy(live::Error::Closed)))
            );
            assert_eq!(app.requests.load(Ordering::Acquire), 0);
            assert_eq!(app.completed, 0);
            assert_eq!(api.whois.load(Ordering::Acquire), 0);
            assert!(supervisor.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert!(UdpSocket::bind(address).is_ok());
            retire(&broker, watch).await;
        })
    });
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn changed_epoch_does_not_allow_restart_after_monitor_failure() {
    struct StopOnCompletion<'a> {
        app: App,
        watch: &'a Watch,
    }
    impl Application for StopOnCompletion<'_> {
        type Output = u64;
        fn request(&mut self, n: u64) -> Result<Request, Error> {
            self.app.request(n)
        }
        fn serve(&mut self, _: Host) -> impl Future<Output = u64> {
            std::future::poll_fn(|_| -> std::task::Poll<u64> { panic!("no peer was sent") })
        }
        fn completed(
            &mut self,
            stats: Statistics,
            outcome: Result<u64, HostError>,
        ) -> Result<Action, Error> {
            assert_eq!(outcome, Err(HostError::Policy(live::Error::Changed)));
            let action = self.app.completed(stats, outcome)?;
            self.watch.stop();
            Ok(action)
        }
    }
    run(|broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let (store, watch, handle) = watched(&broker).await;
            let mut server =
                Server::new(api.client.clone(), identity.clone()).with_live_policy(handle);
            let socket = fixture::listener(&broker).await;
            let address = socket.local_addr();
            let mut app = StopOnCompletion {
                app: App::new(2),
                watch: &watch,
            };
            let requests = app.app.requests.clone();
            let update = async {
                until(&broker, || requests.load(Ordering::Acquire) == 1).await;
                store.update(Change::Approval(Approval::Local)).unwrap();
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime,
                socket,
                policy(),
                fixture::boundary(address, Arc::new(AtomicBool::new(true))),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, update)).await;
            assert_eq!(
                result,
                Err(Error::Host(HostError::Policy(live::Error::Closed)))
            );
            assert_eq!(requests.load(Ordering::Acquire), 1);
            assert_eq!(app.app.completed, 1);
            assert_eq!(app.app.refused, [HostError::Policy(live::Error::Changed)]);
            assert!(UdpSocket::bind(address).is_ok());
            assert!(identity.status(&broker).is_ok());
            retire(&broker, watch).await;
        })
    });
}
