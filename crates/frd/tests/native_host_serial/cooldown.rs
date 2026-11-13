//! Reconnects use real UDP/TLS and the original serial admission path. The
//! LocalAPI, CA and ingress fixtures are not live-tailnet qualification.
use super::*;

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn immediate_ipv4_redial_keeps_its_initial_through_cooldown() {
    immediate_redial(false);
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn immediate_ipv6_redial_keeps_its_initial_through_cooldown() {
    immediate_redial(true);
}

fn immediate_redial(ipv6: bool) {
    run(move |broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::dual_stack();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = Listener::bind_addresses(
                &broker,
                &[
                    "100.64.0.1:0".parse().unwrap(),
                    "[fd7a:115c:a1e0::1]:0".parse().unwrap(),
                ],
                native_accept::Configuration::default(),
            )
            .await
            .unwrap();
            let addresses = listener.local_addresses();
            let next_address = addresses[usize::from(ipv6)];
            let protected = addresses.clone();
            let ingress: IngressCheck = Arc::new(move |bound| protected.contains(&bound));
            let mut app = App::new(2);
            let requests = app.requests.clone();
            let completed = app.completion_signal.clone();
            let policy = Policy {
                cooldown: Duration::from_millis(500),
                ..policy()
            };
            let clients = async {
                let first = runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .unwrap();
                viewer(&first, addresses[0]).await;
                until(&broker, || completed.load(Ordering::Acquire) == 1).await;
                assert_eq!(requests.load(Ordering::Acquire), 1);
                // The service is polled first by both(). It has completed the
                // old peer and yielded in cooldown, before making a new request.
                // The old implementation has NO socket here, on either family.
                for address in &addresses {
                    assert!(
                        network::process_holds_udp(*address),
                        "the protected destination disappeared during cooldown"
                    );
                }
                let whois = api.whois.load(Ordering::Acquire);
                let next = runtime
                    .try_request_cx_with_budget(Budget::INFINITE)
                    .unwrap();
                let mut redial = std::pin::pin!(viewer(&next, next_address));
                std::future::poll_fn(|task| {
                    // Start the actual client immediately, while the host is
                    // still cooling down. No host TLS/admission runs yet.
                    assert!(redial.as_mut().poll(task).is_pending());
                    assert_eq!(requests.load(Ordering::Acquire), 1);
                    assert_eq!(api.whois.load(Ordering::Acquire), whois);
                    std::task::Poll::Ready(())
                })
                .await;
                asupersync::time::timeout(broker.now(), Duration::from_millis(1250), redial)
                    .await
                    .expect("reconnect must not wait for the 1.5-second Initial retransmission");
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime.clone(),
                listener,
                policy,
                ingress,
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, clients)).await;
            assert_eq!(
                result,
                Ok(Statistics {
                    attempts: 2,
                    admitted: 2,
                    refused: 0,
                })
            );
            assert_eq!((app.completed, app.served), (2, 2));
            assert!(app.refused.is_empty());
            assert!(
                app.request_times[1].duration_since(app.completion_times[0]) >= policy.cooldown
            );
            assert!(api.whois.load(Ordering::Acquire) >= 2);
            assert!(supervisor.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            for address in addresses {
                assert!(!network::process_holds_udp(address));
            }
        })
    });
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn ingress_loss_closes_the_prebound_socket_without_a_second_attempt() {
    stop_during_cooldown(true);
}

#[test]
#[ignore = "explicit isolated user/network namespace"]
fn cancellation_closes_the_prebound_socket_without_a_second_attempt() {
    stop_during_cooldown(false);
}

fn stop_during_cooldown(revoke_ingress: bool) {
    run(move |broker, supervisor, runtime| {
        Box::pin(async move {
            let api = fixture::Api::new();
            let identity = api.identity(&broker).await;
            let mut server = Server::new(api.client.clone(), identity.clone());
            let listener = fixture::listener(&broker).await;
            let address = listener.local_addr();
            let live = Arc::new(AtomicBool::new(true));
            let mut app = App::new(2);
            let requests = app.requests.clone();
            let completed = app.completion_signal.clone();
            let client = runtime
                .try_request_cx_with_budget(Budget::INFINITE)
                .unwrap();
            let stop = async {
                viewer(&client, address).await;
                until(&broker, || completed.load(Ordering::Acquire) == 1).await;
                assert_eq!(requests.load(Ordering::Acquire), 1);
                assert!(network::process_holds_udp(address));
                if revoke_ingress {
                    live.store(false, Ordering::Release);
                } else {
                    supervisor.cancel_fast(asupersync::types::CancelKind::User);
                }
            };
            let service = server.serve_serial_on_protected_listener(
                supervisor.clone(),
                runtime,
                listener,
                Policy {
                    cooldown: Duration::from_millis(500),
                    ..policy()
                },
                fixture::boundary(address, live.clone()),
                &mut app,
            );
            let (result, ()) = Box::pin(network::both(service, stop)).await;
            let expected = if revoke_ingress {
                Error::Host(HostError::IngressUnavailable)
            } else {
                Error::Cancelled
            };
            assert_eq!(result, Err(expected));
            assert_eq!(requests.load(Ordering::Acquire), 1);
            assert_eq!((app.completed, app.served), (1, 1));
            assert!(supervisor.is_cancel_requested());
            assert!(!broker.is_cancel_requested());
            assert!(identity.status(&broker).is_ok());
            assert!(!network::process_holds_udp(address));
        })
    });
}
