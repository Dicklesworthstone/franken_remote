//! Real UDP/TLS and production startup; ordinary tests use private peer fixtures.
//! The explicit namespace module exercises `Client::run` and actual `LocalAPI` reads.
use super::*;
use crate::session_startup::test_network as network;
use asupersync::types::Budget;
use fr_core::limits::ProtocolLimits;
use fr_wire::negotiation::Role;
use std::{
    sync::atomic::{AtomicBool, Ordering},
    task::Waker,
};
mod namespace;
fn roots() -> Vec<Certificate> {
    vec![Certificate::from_der(
        std::fs::read(network::pki().join("ca.der")).unwrap(),
    )]
}
fn offer() -> Offer {
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    }
}
#[test]
fn explicit_local_trust_rejects_empty_or_peer_leaf_roots() {
    let api = LocalApi::installed();
    assert!(matches!(
        Client::new(api.clone(), vec![], Duration::from_secs(3)),
        Err(Error::Tailnet(fr_tailnet::Error::InvalidTrustStore))
    ));
    let leaf = Certificate::from_der(std::fs::read(network::pki().join("leaf.der")).unwrap());
    assert!(matches!(
        Client::new(api, vec![leaf], Duration::from_secs(3)),
        Err(Error::Tailnet(fr_tailnet::Error::InvalidTrustStore))
    ));
}
#[test]
fn address_family_selection_uses_only_exact_verified_sets_without_fallback() {
    let local = [
        "100.64.0.1".parse().unwrap(),
        "fd7a:115c:a1e0::1".parse().unwrap(),
    ];
    let peer = [
        "100.64.0.2".parse().unwrap(),
        "fd7a:115c:a1e0::2".parse().unwrap(),
    ];
    for (family, i) in [(AddressFamily::Ipv4, 0), (AddressFamily::Ipv6, 1)] {
        let cfg = Configuration {
            family,
            port: 9443,
            ..Configuration::default()
        };
        let route = select_route(&local, &peer, cfg).unwrap();
        assert_eq!(route.local, local[i]);
        assert_eq!(route.remote, SocketAddr::new(peer[i], 9443));
    }
    assert_eq!(
        select_route(
            &local[..1],
            &peer,
            Configuration {
                family: AddressFamily::Ipv6,
                ..Configuration::default()
            }
        ),
        Err(Error::AddressFamilyUnavailable)
    );
    assert_eq!(
        select_route(
            &local,
            &peer,
            Configuration {
                port: 0,
                ..Configuration::default()
            }
        ),
        Err(Error::InvalidConfiguration)
    );
}
#[test]
fn real_tls_connection_keeps_its_guard_through_approval_and_running_viewer() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hc = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let live = Arc::new(AtomicBool::new(true));
        let (native, host_native) =
            network::native_pair(&cx, "localhost", fr_transport::quic::ALPN).await;
        let check = live.clone();
        let mut viewer = join_viewer(
            &cx,
            native.unwrap(),
            Arc::new(move || check.load(Ordering::Acquire)),
            offer(),
            Configuration::default(),
        )
        .unwrap();
        let mut host = crate::session_startup::connection_test_host(
            hc.clone(),
            host_native.unwrap(),
            offer(),
            Policy::default(),
        );
        let mut approved = false;
        for _ in 0..500 {
            let (a, b) = Box::pin(network::both(
                host.drive(Duration::from_millis(1)),
                viewer.drive(Duration::from_millis(1)),
            ))
            .await;
            a.unwrap();
            b.unwrap();
            if !approved && viewer.approval().is_some() {
                assert!(!viewer.is_complete());
                host.approval().unwrap().decide(true).unwrap();
                approved = true;
            }
            if host.is_complete() && viewer.is_complete() {
                break;
            }
        }
        assert!(approved && host.is_complete() && viewer.is_complete());
        let mut running = viewer.finish().unwrap();
        let (q, _) = running.io().unwrap();
        live.store(false, Ordering::Release);
        assert_eq!(
            q.tick(&cx, || true),
            Err(fr_transport::quic::Error::Unauthorized)
        );
        assert!(q.is_closed());
    });
}
#[test]
fn unpolled_connector_run_cancels_only_its_dedicated_scope() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let unrelated = runtime.request_cx_with_budget(Budget::INFINITE);
    let api = LocalApi::new("/tmp/fr-unpolled-must-not-contact.sock").unwrap();
    let mut client = Client::new(api, roots(), Duration::from_secs(3)).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let app_called = called.clone();
    drop(client.run(
        cx.clone(),
        PeerSelector::Name("unused.fixture.ts.net"),
        Configuration::default(),
        offer(),
        move |_| {
            app_called.store(true, Ordering::Release);
            async {}
        },
    ));
    assert!(cx.checkpoint().is_err());
    assert!(unrelated.checkpoint().is_ok());
    assert!(!called.load(Ordering::Acquire));
}
#[test]
fn incompatible_native_windows_are_rejected_before_localapi_or_socket_work() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let api = LocalApi::new("/tmp/fr-invalid-policy-must-not-contact.sock").unwrap();
        let mut client = Client::new(api, roots(), Duration::from_secs(3)).unwrap();
        let cfg = Configuration {
            transport: Policy {
                stream_window: 16_384,
                ..Policy::default()
            },
            ..Configuration::default()
        };
        let result = client
            .run(
                cx,
                PeerSelector::Name("unused.fixture.ts.net"),
                cfg,
                offer(),
                |_| async { panic!("invalid configuration cannot start application") },
            )
            .await;
        assert!(matches!(result, Err(Error::InvalidConfiguration)));
    });
}
struct PendingApplication {
    cx: Cx,
    dropped: Arc<AtomicBool>,
}
impl Future for PendingApplication {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }
}
impl Drop for PendingApplication {
    fn drop(&mut self) {
        assert!(self.cx.checkpoint().is_err());
        self.dropped.store(true, Ordering::Release);
    }
}
#[test]
fn target_failure_fences_before_dropping_pending_application_work() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let dropped = Arc::new(AtomicBool::new(false));
        let result = run_application(
            &cx,
            async { Err(fr_tailnet::Error::IdentityChanged) },
            PendingApplication {
                cx: cx.clone(),
                dropped: dropped.clone(),
            },
        )
        .await;
        assert_eq!(
            result,
            Err(Error::Tailnet(fr_tailnet::Error::IdentityChanged))
        );
        assert!(dropped.load(Ordering::Acquire));
    });
}
#[test]
fn returning_a_value_ends_target_scope_and_abandoned_scope_fences_before_drop() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        assert_eq!(
            run_application(&cx, std::future::pending(), async { 17 }).await,
            Ok(17)
        );
        assert!(cx.checkpoint().is_err());
    });
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let dropped = Arc::new(AtomicBool::new(false));
    let work = PendingApplication {
        cx: cx.clone(),
        dropped: dropped.clone(),
    };
    let mut task = Context::from_waker(Waker::noop());
    let mut scope = Box::pin(Scoped {
        cx: cx.clone(),
        inner: Box::pin(async move {
            work.await;
            Ok::<_, Error>(())
        }),
    });
    assert!(scope.as_mut().poll(&mut task).is_pending());
    drop(scope);
    assert!(dropped.load(Ordering::Acquire));
}
