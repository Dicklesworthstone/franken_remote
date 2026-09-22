#![cfg(target_os = "linux")]
//! Refusal checks use actual root-credential Unix HTTP and test PKI. They do not
//! install rules, qualify a live tailnet, or claim a successful protected session.
//! Execute explicitly in a disposable user/network namespace as documented in
//! `native_host_accept.rs`; loopback with tailnet-looking addresses must REFUSE.
#[allow(dead_code)]
#[path = "native_host_accept/fixture.rs"]
mod fixture;
#[allow(dead_code)]
#[path = "../../fr-transport/tests/support/mod.rs"]
mod network;
use asupersync::{cx::Cx, types::CancelKind};
use fr_tailnet::{LocalApi, ingress};
use fr_transport::native_accept::{self, Listener};
use frd::native_connection::host::{IngressCheck, LinuxError, Request, Server};
use std::{
    net::{SocketAddr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
fn address() -> SocketAddr {
    "100.64.0.1:4718".parse().unwrap()
}
#[test]
#[ignore = "explicit root in disposable user/network namespace"]
fn installed_address_on_loopback_does_not_qualify_as_tun_ingress() {
    network::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let api = fixture::Api::new();
        let identity = api.identity(&cx).await;
        for interface in ["lo", "fr-nonexistent"] {
            let server = Server::new(api.client.clone(), identity.clone());
            let result = server
                .bind_linux(
                    &cx,
                    ingress::Configuration::new(address(), interface).unwrap(),
                    native_accept::Configuration::default(),
                )
                .await;
            assert!(matches!(
                result,
                Err(LinuxError::Ingress(ingress::Error::UnqualifiedInterface))
            ));
            assert!(UdpSocket::bind(address()).is_ok(), "refusal must not bind");
            assert_eq!(api.whois.load(Ordering::SeqCst), 0);
        }
        assert!(!cx.is_cancel_requested());
    });
}
#[test]
#[ignore = "explicit root in disposable user/network namespace"]
fn address_missing_from_installed_identity_refuses_before_tools_or_binding() {
    network::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let api = fixture::Api::new();
        let identity = api.identity(&cx).await;
        let result = Server::new(api.client.clone(), identity)
            .bind_linux(
                &cx,
                ingress::Configuration::new("100.64.0.3:4718".parse().unwrap(), "lo").unwrap(),
                native_accept::Configuration::default(),
            )
            .await;
        assert!(matches!(
            result,
            Err(LinuxError::Ingress(ingress::Error::InvalidConfiguration))
        ));
        assert_eq!(api.whois.load(Ordering::SeqCst), 0);
        assert!(!cx.is_cancel_requested());
    });
}
#[test]
#[ignore = "explicit root in disposable user/network namespace"]
fn revoked_credentials_refuse_without_further_metadata_or_socket_work() {
    network::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let api = fixture::Api::new();
        let identity = api.identity(&cx).await;
        identity.stop();
        let calls = api.calls.load(Ordering::SeqCst);
        let result = Server::new(api.client.clone(), identity)
            .bind_linux(
                &cx,
                ingress::Configuration::new(address(), "lo").unwrap(),
                native_accept::Configuration::default(),
            )
            .await;
        assert!(matches!(result, Err(LinuxError::Host(_))));
        assert_eq!(api.calls.load(Ordering::SeqCst), calls);
        assert!(UdpSocket::bind(address()).is_ok());
    });
}
#[test]
#[ignore = "explicit root in disposable user/network namespace"]
fn unpolled_bind_drop_never_opens_a_listener_or_revokes_broker_credentials() {
    network::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let api = fixture::Api::new();
        let identity = api.identity(&cx).await;
        let calls = api.calls.load(Ordering::SeqCst);
        drop(
            Server::new(api.client.clone(), identity.clone()).bind_linux(
                &cx,
                ingress::Configuration::new(address(), "lo").unwrap(),
                native_accept::Configuration::default(),
            ),
        );
        assert!(identity.status(&cx).is_ok());
        assert!(!cx.is_cancel_requested());
        assert_eq!(api.calls.load(Ordering::SeqCst), calls);
        assert!(UdpSocket::bind(address()).is_ok());
    });
}
#[test]
#[ignore = "explicit root in disposable user/network namespace"]
fn cancelled_broker_cannot_install_a_boundary_or_listen() {
    network::runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let api = fixture::Api::new();
        let identity = api.identity(&cx).await;
        let calls = api.calls.load(Ordering::SeqCst);
        cx.cancel_fast(CancelKind::User);
        let result = Server::new(api.client.clone(), identity)
            .bind_linux(
                &cx,
                ingress::Configuration::new(address(), "lo").unwrap(),
                native_accept::Configuration::default(),
            )
            .await;
        assert!(matches!(result, Err(LinuxError::Host(_))));
        assert_eq!(api.calls.load(Ordering::SeqCst), calls);
        assert!(UdpSocket::bind(address()).is_ok());
    });
}
