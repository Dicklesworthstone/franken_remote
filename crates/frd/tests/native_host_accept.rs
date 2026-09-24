#![cfg(target_os = "linux")]
//! Real UDP/TLS -> root-credential Unix `LocalAPI` -> real Host/Viewer startup.
//! Metadata, certificate authority and ingress lifetime are explicit fixtures.
//! Run ignored cases inside a disposable user/network namespace, as root there:
//! ```sh
//! unshare -Urn -- sh -c 'ip link set lo up; ip addr add 100.64.0.1/32 dev lo;
//! ip addr add 100.64.0.2/32 dev lo; exec "$@"' sh TEST_BINARY --ignored --test-threads=1
//! ```
//! This never qualifies an installed tailnet or the production ingress boundary.
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
use frd::native_connection::host::{Error, IngressCheck, Request, Server};
use std::{
    cell::Cell,
    future::{Future, pending, poll_fn},
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};
struct Notice<'a>(&'a Cell<bool>);
impl Drop for Notice<'_> {
    fn drop(&mut self) {
        self.0.set(true);
    }
}
fn run(body: impl AsyncFnOnce(Cx, Cx)) {
    let rt = network::runtime();
    let host = rt.request_cx_with_budget(Budget::INFINITE);
    let client = rt.request_cx_with_budget(Budget::INFINITE);
    rt.block_on(async {
        timeout(client.now(), Duration::from_secs(8), body(host, client))
            .await
            .unwrap();
    });
}
#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn cold_client_reaches_real_startup_only_after_membership_and_local_consent() {
    run(async |hc, vc| {
        let api = Api::new();
        let identity = api.identity(&hc).await;
        let mut server = Server::new(api.client.clone(), identity.clone());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let alive = Arc::new(AtomicBool::new(true));
        let notices = Cell::new(0);
        let client_notice = Cell::new(false);
        let operation = server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            boundary(address, alive),
            |host| async {
                assert!(api.whois.load(Ordering::Acquire) > 0);
                assert!(!host.is_complete());
                assert!(host.approval().is_none());
                let mut running = host
                    .open(Duration::from_millis(5), |approval, role| {
                        assert_eq!(role, fr_wire::negotiation::Role::Observe);
                        notices.set(notices.get() + 1);
                        approval.decide(true).unwrap();
                        Ok(())
                    })
                    .await
                    .unwrap();
                assert_eq!(running.binding(), request().session.binding);
                assert!(running.observation().unwrap().check().is_ok());
                assert_eq!(
                    running.selection().role,
                    fr_wire::negotiation::Role::Observe
                );
            },
        );
        let connecting = async {
            let native = client(&vc, address).await;
            let actual_peer = native.local_addr();
            let mut viewer = frd::session_startup::Viewer::new(
                vc.clone(),
                native,
                offer(),
                fr_transport::quic::Policy::default(),
                Duration::from_secs(3),
            )
            .unwrap();
            while !viewer.is_complete() {
                viewer.drive(Duration::from_millis(5)).await.unwrap();
                if viewer.approval().is_some() {
                    client_notice.set(true);
                }
            }
            let session = viewer.finish().unwrap();
            (actual_peer, session)
        };
        let (result, (peer, viewer_session)) = Box::pin(network::both(operation, connecting)).await;
        result.unwrap();
        assert_eq!(notices.get(), 1);
        assert!(client_notice.get());
        assert!(
            api.requests.lock().unwrap().iter().any(
                |path| path == &format!("/localapi/v0/whois?addr=100.64.0.2%3A{}", peer.port())
            )
        );
        assert!(hc.checkpoint().is_err());
        assert!(vc.checkpoint().is_ok());
        assert!(identity.status(&vc).is_ok());
        drop(viewer_session);
        assert!(UdpSocket::bind(address).is_ok());
    });
}
async fn refusal(mode: Mode, expected: fr_tailnet::Error, hc: Cx, vc: Cx) {
    let api = Api::new();
    let identity = api.identity(&hc).await;
    *api.mode.lock().unwrap() = mode;
    let mut server = Server::new(api.client.clone(), identity);
    let socket = listener(&hc).await;
    let address = socket.local_addr();
    let called = Cell::new(false);
    let operation = server.run_on_protected_listener(
        &hc,
        socket,
        request_for(mode),
        boundary(address, Arc::new(AtomicBool::new(true))),
        |_| {
            called.set(true);
            async {}
        },
    );
    let (result, client) = Box::pin(network::both(operation, client(&vc, address))).await;
    assert_eq!(result, Err(Error::Tailnet(expected)));
    assert!(!called.get());
    // LocalAPI retries its bounded status/WhoIs/status snapshot once after
    // the intentional host change; the later node comparison must still refuse.
    assert_eq!(
        api.whois.load(Ordering::Acquire),
        if mode == Mode::ChangeAfterWhoIs { 2 } else { 1 }
    );
    drop(client);
    assert!(hc.checkpoint().is_err());
    assert!(UdpSocket::bind(address).is_ok());
}
#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn other_user_is_refused_before_application_or_approval() {
    run(async |hc, vc| refusal(Mode::OtherUser, fr_tailnet::Error::ScopeDenied, hc, vc).await);
}
#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn absent_membership_evidence_never_falls_back_to_another_profile() {
    run(async |hc, vc| {
        refusal(
            Mode::Unverifiable,
            fr_tailnet::Error::TailnetMembershipUnverifiable,
            hc,
            vc,
        )
        .await;
    });
}
#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn changed_host_identity_after_whois_refuses_the_application_handoff() {
    run(async |hc, vc| {
        refusal(
            Mode::ChangeAfterWhoIs,
            fr_tailnet::Error::IdentityChanged,
            hc,
            vc,
        )
        .await;
    });
}
#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn missing_ingress_fails_before_any_peer_lookup_and_releases_socket() {
    run(async |hc, vc| {
        let api = Api::new();
        let identity = api.identity(&hc).await;
        let before = api.calls.load(Ordering::Acquire);
        let mut server = Server::new(api.client.clone(), identity);
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let result = server
            .run_on_protected_listener(
                &hc,
                socket,
                request(),
                boundary(address, Arc::new(AtomicBool::new(false))),
                |_| async { panic!("unprotected") },
            )
            .await;
        assert_eq!(result, Err(Error::IngressUnavailable));
        assert_eq!(api.calls.load(Ordering::Acquire), before);
        assert_eq!(api.whois.load(Ordering::Acquire), 0);
        assert!(hc.checkpoint().is_err());
        assert!(vc.checkpoint().is_ok());
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn unpolled_acceptance_cancels_only_its_original_scope_without_lookup() {
    run(async |hc, vc| {
        let api = Api::new();
        let identity = api.identity(&hc).await;
        let before = api.calls.load(Ordering::Acquire);
        let mut server = Server::new(api.client.clone(), identity.clone());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        drop(server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            boundary(address, Arc::new(AtomicBool::new(true))),
            |_| async { panic!("abandoned") },
        ));
        assert_eq!(api.calls.load(Ordering::Acquire), before);
        assert!(hc.checkpoint().is_err());
        assert!(vc.checkpoint().is_ok());
        assert!(identity.status(&vc).is_ok());
        assert!(UdpSocket::bind(address).is_ok());
    });
}
#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn credential_stop_wakes_a_parked_application_without_a_new_network_packet() {
    run(async |hc, vc| {
        let api = Api::new();
        let identity = api.identity(&hc).await;
        let mut server = Server::new(api.client.clone(), identity.clone());
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let entered = Cell::new(false);
        let dropped = Cell::new(false);
        let operation = server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            boundary(address, Arc::new(AtomicBool::new(true))),
            |host| async {
                let _host = host;
                let _notice = Notice(&dropped);
                entered.set(true);
                pending::<()>().await;
            },
        );
        let kill = async {
            let client = client(&vc, address).await;
            while !entered.get() {
                sleep(vc.now(), Duration::from_millis(1)).await;
            }
            identity.stop();
            client
        };
        let (result, client) = Box::pin(network::both(operation, kill)).await;
        assert_eq!(result, Err(Error::Tailnet(fr_tailnet::Error::Revoked)));
        assert!(entered.get() && dropped.get());
        assert!(hc.checkpoint().is_err());
        drop(client);
        assert!(UdpSocket::bind(address).is_ok());
    });
}

#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn ingress_gate_survives_host_open_and_fences_io_inside_the_application_poll() {
    run(async |hc, vc| {
        let api = Api::new();
        let identity = api.identity(&hc).await;
        let mut server = Server::new(api.client.clone(), identity);
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let alive = Arc::new(AtomicBool::new(true));
        let checked = Cell::new(false);
        let operation = server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            boundary(address, alive.clone()),
            |host| async {
                let mut running = host
                    .open(Duration::from_millis(5), |a, _| {
                        a.decide(true).unwrap();
                        Ok(())
                    })
                    .await
                    .unwrap();
                let observation = running.observation().unwrap();
                let (transport, _) = running.io().unwrap();
                // Within THIS application poll, before the outer guard can poll:
                alive.store(false, Ordering::Release);
                assert_eq!(
                    transport.tick(&hc, || true),
                    Err(fr_transport::quic::Error::Unauthorized)
                );
                assert!(transport.is_closed());
                assert!(hc.checkpoint().is_err());
                assert!(observation.check().is_err());
                checked.set(true);
                pending::<()>().await;
            },
        );
        let connecting = async {
            let native = client(&vc, address).await;
            let mut viewer = frd::session_startup::Viewer::new(
                vc.clone(),
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
        };
        let (result, viewer) = Box::pin(network::both(operation, connecting)).await;
        assert_eq!(result, Err(Error::Cancelled));
        assert!(checked.get());
        drop(viewer);
        assert!(UdpSocket::bind(address).is_ok());
    });
}

#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn caught_application_panic_fences_before_the_failed_future_is_dropped() {
    run(async |hc, vc| {
        let api = Api::new();
        let identity = api.identity(&hc).await;
        let mut server = Server::new(api.client.clone(), identity);
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let held = std::cell::RefCell::new(None);
        let mut operation = Box::pin(server.run_on_protected_listener(
            &hc,
            socket,
            request(),
            boundary(address, Arc::new(AtomicBool::new(true))),
            |host| {
                *held.borrow_mut() = Some(host);
                async { panic!("local application fixture panic") }
            },
        ));
        let caught = async {
            poll_fn(|task| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    operation.as_mut().poll(task)
                }));
                match result {
                    Err(_) => Poll::Ready(()),
                    Ok(Poll::Pending) => Poll::Pending,
                    Ok(Poll::Ready(result)) => panic!("expected callback panic, got {result:?}"),
                }
            })
            .await;
            // The failed serving future is still retained, not dropped by catch.
            assert!(hc.checkpoint().is_err());
        };
        let ((), client) = Box::pin(network::both(caught, client(&vc, address))).await;
        // Host is still held outside the failed future: its destructor cannot
        // be the reason the earlier cancellation assertion passed.
        assert!(held.borrow().is_some());
        drop(operation);
        drop(held.borrow_mut().take());
        drop(client);
        assert!(UdpSocket::bind(address).is_ok());
    });
}

#[test]
#[ignore = "requires disposable root user/network namespace with two assigned fixture addresses"]
fn incompatible_native_credit_is_rejected_before_lookup_or_application() {
    run(async |hc, vc| {
        let api = Api::new();
        let identity = api.identity(&hc).await;
        let before = api.calls.load(Ordering::Acquire);
        let mut server = Server::new(api.client.clone(), identity);
        let socket = listener(&hc).await;
        let address = socket.local_addr();
        let mut invalid = request();
        invalid.session.transport.stream_window = 16_384;
        let result = server
            .run_on_protected_listener(
                &hc,
                socket,
                invalid,
                boundary(address, Arc::new(AtomicBool::new(true))),
                |_| async { panic!("credit already advertised cannot be retracted") },
            )
            .await;
        assert_eq!(
            result,
            Err(Error::Session(
                frd::session_startup::Error::InvalidConfiguration
            ))
        );
        assert_eq!(api.calls.load(Ordering::Acquire), before);
        assert!(hc.checkpoint().is_err());
        assert!(vc.checkpoint().is_ok());
        assert!(UdpSocket::bind(address).is_ok());
    });
}
