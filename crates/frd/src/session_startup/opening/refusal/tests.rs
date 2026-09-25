//! Actual native QUIC/TLS/UDP and the production viewer state machine, with
//! test-only identity admission. These are not installed-tailnet qualification.
use super::*;
use crate::session_startup::{
    Peer, Viewer, WAITING, connection_test_host, test_network as network,
};
use asupersync::types::Budget;
use fr_core::limits::ProtocolLimits;
use fr_wire::negotiation::{Capability, Offer};
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

fn exchange(client_offer: Offer, observer_only: bool, expected: Reason, local: Error) {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
        // One retained critical record forces the denial to respect any pending
        // capabilities/approval notice rather than overwrite or bypass it.
        let policy = quic::Policy {
            critical_send_records: 1,
            ..quic::Policy::default()
        };
        let mut host = connection_test_host(hcx.clone(), server.unwrap(), offer(), policy);
        if observer_only {
            host.restrict_shared_observer().unwrap();
        }
        let alive = match host.peer.as_ref().unwrap() {
            Peer::Fixture { alive, .. } => Arc::clone(alive),
            Peer::Tailnet(_) => unreachable!("private test admission only"),
        };
        let mut viewer = Viewer::new(
            cx.clone(),
            client.unwrap(),
            client_offer,
            policy,
            Duration::from_secs(2),
        )
        .unwrap();
        let notices = Cell::new(0);
        let host_run = host.open(Duration::from_millis(1), |approval, _| {
            notices.set(notices.get() + 1);
            approval.decide(false).unwrap();
            Ok(())
        });
        let viewer_run = async {
            for _ in 0..1000 {
                if let Err(error) = viewer.drive(Duration::from_millis(1)).await {
                    return error;
                }
                assert!(!viewer.is_complete(), "a refusal must not open a session");
            }
            panic!("viewer did not receive a terminal result");
        };
        let (host_result, client_result) = Box::pin(network::both(host_run, viewer_run)).await;
        assert!(matches!(host_result, Err(error) if error == local));
        assert_eq!(
            client_result,
            Error::ClientStartup(fr_client::startup::Error::Protocol(
                negotiation::Error::Refused(Refused::connection(expected))
            ))
        );
        assert_eq!(
            notices.get(),
            usize::from(expected == Reason::LocalApprovalDenied)
        );
        assert!(!alive.load(Ordering::Acquire));
        assert!(!viewer.is_complete());
    });
}

#[test]
fn native_viewer_receives_control_unavailable_without_a_consent_prompt() {
    let mut client = offer();
    client.role = Role::RequestControl;
    exchange(client, true, Reason::ControlUnavailable, Error::Denied);
}

#[test]
fn native_viewer_receives_local_consent_denial_under_one_record_credit() {
    exchange(offer(), false, Reason::LocalApprovalDenied, Error::Denied);
}

#[test]
fn native_viewer_receives_version_refusal_not_an_unexplained_disconnect() {
    let mut client = offer();
    client.versions = vec![1];
    exchange(
        client,
        false,
        Reason::UnsupportedVersion,
        Error::Protocol(negotiation::Error::Version),
    );
}

#[test]
fn native_viewer_receives_missing_required_capability_before_approval() {
    let mut client = offer();
    client.capabilities.push(Capability {
        name: "unavailable-feature".into(),
        version: 1,
        required: true,
    });
    exchange(
        client,
        false,
        Reason::RequiredCapability,
        Error::Protocol(negotiation::Error::RequiredCapability),
    );
}

#[test]
fn terminal_fence_retires_consent_before_reporting_io() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
        let _client = client.unwrap();
        let mut host = connection_test_host(
            cx.clone(),
            server.unwrap(),
            offer(),
            quic::Policy::default(),
        );
        host.phase = Phase::Approval;
        host.approval.store(WAITING, Ordering::Release);
        host.bytes.fill(1);
        host.len = 10;
        let approval = host.approval().unwrap();
        fence(&mut host);
        assert_eq!(host.check(), Err(Error::Closed));
        assert!(approval.decide(true).is_err());
        assert_eq!(host.len, 0);
        assert!(host.bytes.iter().all(|byte| *byte == 0));
        // Only the original transport remains available for the bounded report.
        assert!(!host.transport.as_ref().unwrap().is_closed());
        host.close();
        assert!(host.transport.as_ref().unwrap().is_closed());
    });
}

#[test]
fn bound_or_revoked_sessions_cannot_use_the_bootstrap_reporting_path() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        for bound in [false, true] {
            let (client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
            let _client = client.unwrap();
            let mut host = connection_test_host(
                cx.clone(),
                server.unwrap(),
                offer(),
                quic::Policy::default(),
            );
            if bound {
                host.phase = Phase::Bind;
            } else {
                host.peer.as_ref().unwrap().revoke();
            }
            report(&mut host, Error::Denied).await;
            assert!(host.transport.as_ref().unwrap().is_closed());
            assert_eq!(
                host.transport
                    .as_ref()
                    .unwrap()
                    .usage()
                    .retained_send_records,
                0
            );
            assert_eq!(host.check(), Err(Error::Closed));
        }
    });
}

#[test]
fn reasons_preserve_failure_categories_and_never_answer_a_refusal() {
    for (error, expected) in [
        (negotiation::Error::Profile, Reason::UnsupportedProfile),
        (negotiation::Error::Limits, Reason::InvalidLimits),
        (negotiation::Error::Selection, Reason::InvalidSelection),
        (negotiation::Error::Allocation, Reason::ResourceLimit),
        (negotiation::Error::Invalid, Reason::InvalidMessage),
    ] {
        assert_eq!(
            reason(Phase::Hello, false, Error::Protocol(error)),
            Some(expected)
        );
    }
    assert_eq!(
        reason(Phase::Approval, false, Error::Expired),
        Some(Reason::ApprovalExpired)
    );
    assert_eq!(
        reason(Phase::Hello, false, Error::Denied),
        Some(Reason::PermissionDenied)
    );
    for error in [
        Error::Cancelled,
        Error::Closed,
        Error::Transport(quic::Error::Closed),
        Error::Protocol(negotiation::Error::Refused(Refused::connection(
            Reason::InvalidMessage,
        ))),
    ] {
        assert_eq!(reason(Phase::Hello, false, error), None);
    }
}

#[test]
fn abandoning_an_unpolled_open_revokes_the_original_peer() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
        let _client = client.unwrap();
        let host = connection_test_host(
            cx.clone(),
            server.unwrap(),
            offer(),
            quic::Policy::default(),
        );
        let alive = match host.peer.as_ref().unwrap() {
            Peer::Fixture { alive, .. } => Arc::clone(alive),
            Peer::Tailnet(_) => unreachable!("private test admission only"),
        };
        let open = host.open(Duration::from_millis(1), |_, _| panic!("never polled"));
        assert!(alive.load(Ordering::Acquire));
        drop(open);
        assert!(!alive.load(Ordering::Acquire));
    });
}
