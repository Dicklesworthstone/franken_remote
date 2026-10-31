//! Actual native UDP/TLS with private fixture identity, not installed-tailnet
//! qualification. No worker, display discovery, approval, or session is opened.
use super::*;
use crate::session_startup::{Peer, Viewer, connection_test_host, test_network as network};
use asupersync::types::Budget;
use fr_core::limits::ProtocolLimits;
use fr_wire::negotiation::Offer;
use std::{cell::Cell, sync::Arc};

fn offer() -> Offer {
    Offer {
        versions: vec![0],
        profile: negotiation::NATIVE_PROFILE,
        profile_version: negotiation::PROFILE_VERSION,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: vec![],
    }
}

#[test]
fn native_inspection_receives_terminal_ack_without_an_approval_or_session() {
    for require_approval in [false, true] {
        let runtime = network::runtime();
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
        runtime.block_on(async {
            let (client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
            let policy = quic::Policy {
                critical_send_records: 1,
                ..quic::Policy::default()
            };
            let mut host = connection_test_host(hcx, server.unwrap(), offer(), policy);
            host.config.require_approval = require_approval;
            let alive = match host.peer.as_ref().unwrap() {
                Peer::Fixture { alive, .. } => Arc::clone(alive),
                Peer::Tailnet(_) => unreachable!("fixture admission only"),
            };
            let offered = inspection::prepare_offer(offer()).unwrap();
            let viewer = Viewer::new(
                cx.clone(),
                client.unwrap(),
                offered.clone(),
                policy,
                Duration::from_secs(5),
            )
            .unwrap();
            let notices = Cell::new(0);
            let host_run = host.open(Duration::from_millis(1), |_, _| {
                notices.set(notices.get() + 1);
                Err(())
            });
            let (host_result, client_result) =
                Box::pin(network::both(host_run, viewer.inspect_capabilities())).await;
            assert!(matches!(host_result, Err(Error::InspectionComplete)));
            assert_eq!(client_result, Ok(offered));
            assert_eq!(notices.get(), 0);
            assert!(!alive.load(Ordering::Acquire));
            assert!(
                cx.checkpoint().is_err(),
                "the probe retained its transport owner"
            );
        });
    }
}

#[test]
fn a_negotiated_probe_cannot_select_a_session_even_when_local_approval_is_disabled() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
        let policy = quic::Policy {
            critical_send_records: 1,
            ..quic::Policy::default()
        };
        let mut host = connection_test_host(hcx, server.unwrap(), offer(), policy);
        host.config.require_approval = false;
        let mut viewer = Viewer::new(
            cx.clone(),
            client.unwrap(),
            inspection::prepare_offer(offer()).unwrap(),
            policy,
            Duration::from_secs(2),
        )
        .unwrap();
        let notices = Cell::new(0);
        let host_run = host.open(Duration::from_millis(1), |_, _| {
            notices.set(notices.get() + 1);
            Err(())
        });
        // Deliberately use ordinary startup despite the required marker: its
        // SelectedConfiguration must be refused, not interpreted as approval.
        let viewer_run = async {
            for _ in 0..1000 {
                if let Err(error) = viewer.drive(Duration::from_millis(1)).await {
                    return error;
                }
                assert!(!viewer.is_complete());
            }
            panic!("inspection selection did not terminate");
        };
        let (host_result, client_error) = Box::pin(network::both(host_run, viewer_run)).await;
        assert!(matches!(host_result, Err(Error::InspectionAbandoned)));
        assert!(matches!(
            client_error,
            Error::ClientStartup(fr_client::startup::Error::Protocol(
                negotiation::Error::Refused(_)
            ))
        ));
        assert_eq!(notices.get(), 0);
        assert!(!viewer.is_complete());
    });
}

#[test]
fn dropping_an_unpolled_probe_cancels_its_original_viewer() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
        let viewer = Viewer::new(
            cx.clone(),
            client.unwrap(),
            inspection::prepare_offer(offer()).unwrap(),
            quic::Policy::default(),
            Duration::from_secs(2),
        )
        .unwrap();
        let probe = viewer.inspect_capabilities();
        assert!(cx.checkpoint().is_ok());
        drop(probe);
        assert!(cx.checkpoint().is_err());
        drop(server);
    });
}

#[test]
fn terminal_probe_classification_preserves_local_faults_and_clean_departure() {
    for error in [
        Error::Clock,
        Error::Cancelled,
        Error::InvalidConfiguration,
        Error::Authority,
        Error::Protocol(negotiation::Error::Allocation),
        Error::Transport(quic::Error::Clock),
        Error::Transport(quic::Error::Cancelled),
        Error::Transport(quic::Error::InvalidPolicy),
        Error::Transport(quic::Error::Allocation),
        Error::Transport(quic::Error::Stream(
            fr_wire::stream::StreamError::ClockRegression,
        )),
    ] {
        assert_eq!(terminal_error(error), error);
    }
    for error in [
        Error::Expired,
        Error::Order,
        Error::Denied,
        Error::Closed,
        Error::Protocol(negotiation::Error::Selection),
        Error::Transport(quic::Error::Backpressure),
        Error::Transport(quic::Error::Expired),
    ] {
        assert_eq!(terminal_error(error), Error::InspectionAbandoned);
    }
    assert!(Error::InspectionComplete.is_peer_departure());
    assert!(!Error::InspectionAbandoned.is_peer_departure());
}
