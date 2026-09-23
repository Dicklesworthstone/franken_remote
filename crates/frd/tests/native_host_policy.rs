#![cfg(target_os = "linux")]
//! Real disk policy, UDP/TLS, credential-checked Unix HTTP and Host negotiation.
//! `LocalAPI` metadata, CA and ingress assertion are explicit fixtures. Execute in
//! the disposable root user/network namespace documented in `native_host_accept.rs`.
#[allow(dead_code)]
#[path = "native_host_accept/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../../fr-transport/tests/support/mod.rs"]
mod network;
use asupersync::{
    cx::Cx,
    time::{sleep, timeout},
    types::Budget,
};
use fixture::*;
use fr_tailnet::LocalApi;
use fr_transport::native_accept::{self, Listener};
use frd::{
    host_policy::{
        Approval, Change, Sharing, Store,
        live::{self, Handle, Status, Watch},
    },
    native_connection::host::{Error, IngressCheck, Request, Server},
};
use std::{
    cell::Cell,
    future::pending,
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

static SERIAL: Mutex<()> = Mutex::new(());
fn run(body: impl AsyncFnOnce(Cx, Cx, Cx)) {
    let _serial = SERIAL.lock().unwrap();
    let rt = network::runtime();
    let broker = rt.request_cx_with_budget(Budget::INFINITE);
    let host = rt.request_cx_with_budget(Budget::INFINITE);
    let client = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        timeout(
            broker.now(),
            Duration::from_secs(8),
            body(broker.clone(), host, client),
        )
        .await
        .unwrap();
    });
}
fn store() -> Store {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    Store::new(&pki().join(format!("policy-{}", NEXT.fetch_add(1, Ordering::Relaxed)))).unwrap()
}
async fn wait(cx: &Cx, mut condition: impl FnMut() -> bool) {
    timeout(cx.now(), Duration::from_secs(2), async {
        while !condition() {
            sleep(cx.now(), Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}
async fn active(cx: &Cx, handle: &Handle) {
    wait(cx, || handle.status() != Status::Opening).await;
    assert!(
        matches!(handle.status(), Status::Active(_)),
        "{:?}",
        handle.status()
    );
}
async fn finish(cx: &Cx, watch: &mut Watch) {
    watch.stop();
    wait(cx, || {
        watch.try_finish().is_some_and(|result| {
            result.unwrap();
            true
        })
    })
    .await;
}
fn protected(address: SocketAddr) -> IngressCheck {
    boundary(address, Arc::new(AtomicBool::new(true)))
}
async fn viewer(cx: &Cx, address: SocketAddr) -> frd::session_startup::ViewerSession {
    let native = client(cx, address).await;
    let mut viewer = frd::session_startup::Viewer::new(
        cx.clone(),
        native,
        offer(),
        fr_transport::quic::Policy::default(),
        Duration::from_secs(3),
    )
    .unwrap();
    while !viewer.is_complete() {
        viewer.drive(Duration::from_millis(5)).await.unwrap();
    }
    viewer.finish().unwrap()
}

#[test]
#[ignore = "explicit disposable root user/network namespace; see module documentation"]
fn live_own_user_scope_overrides_a_broader_request_before_application() {
    run(async |bc, hc, vc| {
        let api = Api::new();
        let identity = api.identity(&bc).await;
        *api.mode.lock().unwrap() = Mode::OtherUser;
        let mut watch = Watch::start(&bc, store()).unwrap();
        active(&bc, &watch.handle()).await;
        let mut server =
            Server::new(api.client.clone(), identity.clone()).with_live_policy(watch.handle());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let mut request = request();
        request.admission.scope = fr_tailnet::Scope::Tailnet;
        let operation =
            server.run_on_protected_listener(&hc, socket, request, protected(address), |_| async {
                panic!("broader request escaped local own-user policy")
            });
        let (result, native) = Box::pin(network::both(operation, client(&vc, address))).await;
        assert_eq!(result, Err(Error::Tailnet(fr_tailnet::Error::ScopeDenied)));
        assert_eq!(api.whois.load(Ordering::Acquire), 1);
        assert!(hc.is_cancel_requested());
        assert!(!bc.is_cancel_requested());
        assert!(identity.status(&bc).is_ok());
        assert!(matches!(watch.handle().status(), Status::Active(_)));
        drop(native);
        assert!(UdpSocket::bind(address).is_ok());
        finish(&bc, &mut watch).await;
    });
}
#[test]
#[ignore = "explicit disposable root user/network namespace; see module documentation"]
fn explicit_local_tailnet_policy_is_used_for_new_connections() {
    run(async |bc, hc, vc| {
        let api = Api::new();
        let identity = api.identity(&bc).await;
        *api.mode.lock().unwrap() = Mode::OtherUser;
        let disk = store();
        disk.update(Change::Sharing(Sharing::Tailnet)).unwrap();
        let mut watch = Watch::start(&bc, disk).unwrap();
        active(&bc, &watch.handle()).await;
        let mut server = Server::new(api.client.clone(), identity).with_live_policy(watch.handle());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let operation = server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            protected(address),
            |host| async {
                let original = host;
                assert!(!original.is_complete());
                assert!(api.whois.load(Ordering::Acquire) > 0);
                17
            },
        );
        let (result, native) = Box::pin(network::both(operation, client(&vc, address))).await;
        assert_eq!(result, Ok(17));
        assert!(!bc.is_cancel_requested());
        drop(native);
        finish(&bc, &mut watch).await;
    });
}
#[test]
#[ignore = "explicit disposable root user/network namespace; see module documentation"]
fn local_approval_policy_cannot_be_bypassed_by_request_flag() {
    run(async |bc, hc, vc| {
        let api = Api::new();
        let identity = api.identity(&bc).await;
        let disk = store();
        disk.update(Change::Approval(Approval::Local)).unwrap();
        let mut watch = Watch::start(&bc, disk).unwrap();
        active(&bc, &watch.handle()).await;
        let mut server = Server::new(api.client.clone(), identity).with_live_policy(watch.handle());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let mut req = request();
        req.session.require_approval = false;
        let notices = Cell::new(0);
        let operation =
            server.run_on_protected_listener(&hc, socket, req, protected(address), |host| async {
                let mut session = host
                    .open(Duration::from_millis(5), |approval, role| {
                        assert_eq!(role, fr_wire::negotiation::Role::Observe);
                        notices.set(notices.get() + 1);
                        approval.decide(true).unwrap();
                        Ok(())
                    })
                    .await
                    .unwrap();
                assert_eq!(session.binding(), request().session.binding);
                assert!(session.observation().unwrap().check().is_ok());
            });
        let (result, viewer) = Box::pin(network::both(operation, viewer(&vc, address))).await;
        assert_eq!(result, Ok(()));
        assert_eq!(notices.get(), 1);
        drop(viewer);
        assert!(!bc.is_cancel_requested());
        finish(&bc, &mut watch).await;
    });
}
#[test]
#[ignore = "explicit disposable root user/network namespace; see module documentation"]
fn a_revision_change_ends_a_parked_authenticated_application_without_remote_io() {
    run(async |bc, hc, vc| {
        let api = Api::new();
        let identity = api.identity(&bc).await;
        // Keep one exact store for the real management write and the monitor.
        let path = pki().join("parked-live-policy-active");
        let disk = Store::new(&path).unwrap();
        let mut watch = Watch::start(&bc, Store::new(&path).unwrap()).unwrap();
        active(&bc, &watch.handle()).await;
        let mut server =
            Server::new(api.client.clone(), identity.clone()).with_live_policy(watch.handle());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let entered = Cell::new(false);
        let mut operation = Box::pin(server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            protected(address),
            |host| async {
                let _original = host;
                entered.set(true);
                pending::<()>().await;
            },
        ));
        let connecting = async {
            let native = client(&vc, address).await;
            wait(&bc, || entered.get()).await;
            let saved = disk
                .update(Change::Approval(Approval::Local))
                .unwrap()
                .policy;
            wait(&bc, || watch.handle().status() == Status::Active(saved)).await;
            native
        };
        let (result, native) = Box::pin(network::both(operation.as_mut(), connecting)).await;
        assert_eq!(result, Err(Error::Policy(live::Error::Changed)));
        assert!(
            hc.is_cancel_requested(),
            "completed future is still retained"
        );
        assert!(!bc.is_cancel_requested());
        assert!(identity.status(&bc).is_ok());
        assert_eq!(api.whois.load(Ordering::Acquire), 1);
        drop(operation);
        drop(native);
        assert!(UdpSocket::bind(address).is_ok());
        finish(&bc, &mut watch).await;
    });
}
#[test]
#[ignore = "explicit disposable root user/network namespace; see module documentation"]
fn unpolled_acceptance_cannot_take_a_new_policy_epoch_after_local_change() {
    run(async |bc, hc, vc| {
        let api = Api::new();
        let identity = api.identity(&bc).await;
        let path = pki().join("unpolled-live-policy");
        let disk = Store::new(&path).unwrap();
        let mut watch = Watch::start(&bc, Store::new(&path).unwrap()).unwrap();
        active(&bc, &watch.handle()).await;
        let mut server = Server::new(api.client.clone(), identity).with_live_policy(watch.handle());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let before = api.calls.load(Ordering::Acquire);
        let operation = server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            protected(address),
            |_| async { panic!("replaced policy") },
        );
        let saved = disk
            .update(Change::Approval(Approval::Local))
            .unwrap()
            .policy;
        wait(&bc, || watch.handle().status() == Status::Active(saved)).await;
        assert_eq!(operation.await, Err(Error::Policy(live::Error::Changed)));
        assert_eq!(api.calls.load(Ordering::Acquire), before);
        assert_eq!(api.whois.load(Ordering::Acquire), 0);
        assert!(hc.is_cancel_requested());
        assert!(!bc.is_cancel_requested());
        assert!(!vc.is_cancel_requested());
        assert!(UdpSocket::bind(address).is_ok());
        finish(&bc, &mut watch).await;
    });
}
#[test]
#[ignore = "explicit disposable root user/network namespace; see module documentation"]
fn stopped_policy_has_no_fallback_to_caller_selected_permissions() {
    run(async |bc, hc, _| {
        let api = Api::new();
        let identity = api.identity(&bc).await;
        let mut watch = Watch::start(&bc, store()).unwrap();
        active(&bc, &watch.handle()).await;
        let mut server = Server::new(api.client.clone(), identity).with_live_policy(watch.handle());
        watch.stop();
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let before = api.calls.load(Ordering::Acquire);
        assert_eq!(
            server
                .run_on_protected_listener(&hc, socket, request(), protected(address), |_| async {
                    panic!("stopped watch")
                })
                .await,
            Err(Error::Policy(live::Error::Closed))
        );
        assert_eq!(api.calls.load(Ordering::Acquire), before);
        assert_eq!(api.whois.load(Ordering::Acquire), 0);
        assert!(hc.is_cancel_requested());
        assert!(!bc.is_cancel_requested());
        assert!(UdpSocket::bind(address).is_ok());
        finish(&bc, &mut watch).await;
    });
}
#[test]
#[ignore = "explicit disposable root user/network namespace; see module documentation"]
fn original_transport_guard_survives_host_session_handoff_and_checks_before_outer_repoll() {
    run(async |bc, hc, vc| {
        let api = Api::new();
        let identity = api.identity(&bc).await;
        let mut watch = Watch::start(&bc, store()).unwrap();
        active(&bc, &watch.handle()).await;
        let mut server = Server::new(api.client.clone(), identity).with_live_policy(watch.handle());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let operation = server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            protected(address),
            |host| async {
                let mut session = host
                    .open(Duration::from_millis(5), |_, _| {
                        panic!("default policy requires no product prompt")
                    })
                    .await
                    .unwrap();
                let (q, _) = session.io().unwrap();
                assert!(q.tick(&hc, || true).is_ok());
                watch.stop();
                assert!(
                    q.tick(&hc, || true).is_err(),
                    "lease must remain in ORIGINAL transport"
                );
                assert!(
                    hc.is_cancel_requested(),
                    "guard ran inside the same application poll"
                );
                assert!(!bc.is_cancel_requested());
                71
            },
        );
        let (result, viewer) = Box::pin(network::both(operation, viewer(&vc, address))).await;
        assert_eq!(
            result,
            Ok(71),
            "completed application result is not rewritten as rollback"
        );
        drop(viewer);
        finish(&bc, &mut watch).await;
    });
}
