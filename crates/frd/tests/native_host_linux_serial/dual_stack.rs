//! Real IPv4/IPv6 UDP/TLS and host admission with explicitly synthetic ingress.
use super::*;

pub(super) fn ipv6_address() -> SocketAddr {
    "[fd7a:115c:a1e0::1]:4791".parse().unwrap()
}

pub(super) fn tools() -> Tools {
    Tools::profile(include_str!("dual_stack_tools.py"))
}

async fn negotiate_at(cx: Cx, address: SocketAddr) -> ViewerSession {
    let native = fixture::client(&cx, address).await;
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

#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn successive_ipv6_and_ipv4_peers_keep_both_original_protections_renewed() {
    run(async |broker, supervisor, client, handle| {
        let tools = tools();
        let api = fixture::Api::dual_stack();
        let identity = api.identity(&broker).await;
        let mut server = Server::new(api.client.clone(), identity.clone())
            .bind_linux_addresses(
                &broker,
                vec![
                    tools.configuration(),
                    tools.configuration_for(ipv6_address()),
                ],
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
        assert_eq!(server.addresses(), [address(), ipv6_address()]);
        assert_eq!(server.cleanup_tables().len(), 2);
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
            let first = negotiate_at(
                handle.try_request_cx_with_budget(Budget::INFINITE).unwrap(),
                ipv6_address(),
            )
            .await;
            until(&client, || completed.load(Ordering::Acquire) == 1).await;
            drop(first);
            // Exceed the original three-second node evidence lifetime. Both
            // boundaries must renew while only one family's peer is active.
            sleep(client.now(), Duration::from_millis(3300)).await;
            negotiate_at(
                handle.try_request_cx_with_budget(Budget::INFINITE).unwrap(),
                address(),
            )
            .await
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
        assert_eq!(tools.state()["created"], 2);
        for ip in ["100.64.0.1", "fd7a:115c:a1e0::1"] {
            assert!(tools.state()["reads"][ip].as_u64().unwrap() >= 5);
        }
        assert_eq!(tools.state()["deleted"], 0);
        drop(second);
        server.stop(&broker).await.unwrap();
        server.stop(&broker).await.unwrap();
        assert_eq!(tools.state()["deleted"], 2);
        assert_eq!(tools.state()["tables"], serde_json::json!({}));
        for address in server.addresses() {
            assert!(UdpSocket::bind(address).is_ok());
        }
        assert!(
            api.requests.lock().unwrap().iter().any(|request| request
                .starts_with("/localapi/v0/whois?addr=%5Bfd7a%3A115c%3Aa1e0%3A%3A2%5D%3A")),
            "the IPv6 transport peer must be admitted by its exact endpoint"
        );
    });
}

#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn tampering_with_either_protection_fences_the_whole_idle_listener_set() {
    for affected in ["100.64.0.1", "fd7a:115c:a1e0::1"] {
        run(async |broker, supervisor, client, handle| {
            let tools = tools();
            let api = fixture::Api::dual_stack();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone())
                .bind_linux_addresses(
                    &broker,
                    vec![
                        tools.configuration(),
                        tools.configuration_for(ipv6_address()),
                    ],
                    native_accept::Configuration::default(),
                )
                .await
                .unwrap();
            let mut app = App {
                stop_after: 9,
                ..App::default()
            };
            let serving = server.serve_serial(
                supervisor.clone(),
                handle,
                serial::Policy::default(),
                &mut app,
            );
            let tamper = async {
                sleep(client.now(), Duration::from_millis(100)).await;
                fs::write(tools.0.join("tamper"), affected).unwrap();
            };
            let (result, ()) = Box::pin(network::both(serving, tamper)).await;
            assert_eq!(
                result,
                Err(LinuxError::Ingress(ingress::Error::FirewallMismatch))
            );
            assert!(supervisor.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert!(app.outcomes.is_empty());
            assert_eq!(api.whois.load(Ordering::Acquire), 0);
            for address in server.addresses() {
                assert!(UdpSocket::bind(address).is_ok());
            }
            server.stop(&broker).await.unwrap();
            assert_eq!(tools.state()["deleted"], 2);
        });
    }
}

#[test]
#[ignore = "explicit synthetic sysfs/nft namespace runner"]
fn a_failed_second_bind_cleans_all_installed_protections_without_a_partial_listener() {
    run(async |broker, _, _, _| {
        let tools = tools();
        let api = fixture::Api::dual_stack();
        let identity = api.identity(&broker).await;
        let occupied = UdpSocket::bind(ipv6_address()).unwrap();
        let result = Server::new(api.client.clone(), identity.clone())
            .bind_linux_addresses(
                &broker,
                vec![
                    tools.configuration(),
                    tools.configuration_for(ipv6_address()),
                ],
                native_accept::Configuration::default(),
            )
            .await;
        assert!(matches!(
            result,
            Err(LinuxError::Host(HostError::Accept(
                native_accept::Error::Endpoint
            )))
        ));
        assert_eq!(tools.state()["created"], 2);
        assert_eq!(tools.state()["deleted"], 2);
        assert_eq!(tools.state()["tables"], serde_json::json!({}));
        assert!(UdpSocket::bind(address()).is_ok());
        assert!(identity.status(&broker).is_ok());
        assert!(!broker.is_cancel_requested());
        assert_eq!(api.whois.load(Ordering::Acquire), 0);
        drop(occupied);
        assert!(UdpSocket::bind(ipv6_address()).is_ok());
    });
}
