//! Actual native UDP/TLS with private fixture identity (the directory itself
//! comes from a fixture; the installed-daemon read has its own tests in
//! fr-tailnet). No worker, display discovery, approval, or session is opened.
use super::*;
use crate::session_startup::{Peer, Viewer, connection_test_host, test_network as network};
use asupersync::types::Budget;
use fr_core::limits::ProtocolLimits;
use fr_tailnet::{Directory, DirectoryNode};
use fr_wire::negotiation::{self, Offer, Role};
use fr_wire::refusal::Refused;
use std::{
    cell::Cell,
    net::{Ipv4Addr, Ipv6Addr},
    sync::{Arc, atomic::Ordering},
};

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
fn node(i: u8, serving: bool) -> DirectoryNode {
    DirectoryNode {
        name: format!("m{i}.tail1f21e.ts.net"),
        node_id: format!("n{i}CNTRL"),
        os: "macOS".into(),
        online: serving || i.is_multiple_of(2),
        serving,
        ipv4: Some(Ipv4Addr::new(100, 70, 0, i)),
        ipv6: (!i.is_multiple_of(3))
            .then(|| Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, u16::from(i))),
    }
}
fn entry(node: &DirectoryNode) -> Entry {
    Entry {
        name: node.name.clone(),
        node_id: node.node_id.clone(),
        os: node.os.clone(),
        online: node.online,
        serving: node.serving,
        ipv4: node.ipv4,
        ipv6: node.ipv6,
    }
}
fn refused(reason: Reason) -> Error {
    Error::ClientStartup(fr_client::startup::Error::Protocol(
        negotiation::Error::Refused(Refused::connection(reason)),
    ))
}

/// One fetch against a fixture host; returns (host result, viewer result,
/// approval notices, the fixture admission still alive).
fn fetch(
    enabled: bool,
    fixture: Option<Result<Directory, fr_tailnet::Error>>,
) -> (Result<(), Error>, Result<Listing, Error>, usize, bool) {
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
        // Approval disabled: the directory must still never select a session.
        host.config.require_approval = false;
        if enabled {
            host.serve_directory().unwrap();
        }
        host.directory_fixture = fixture;
        let alive = match host.peer.as_ref().unwrap() {
            Peer::Fixture { alive, .. } => Arc::clone(alive),
            Peer::Tailnet(_) => unreachable!("fixture admission only"),
        };
        let viewer = Viewer::new(
            cx.clone(),
            client.unwrap(),
            directory::prepare_offer(offer()).unwrap(),
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
            Box::pin(network::both(host_run, viewer.fetch_directory())).await;
        assert!(
            cx.checkpoint().is_err(),
            "the fetch retained its transport owner"
        );
        (
            host_result.map(|_| ()),
            client_result,
            notices.get(),
            alive.load(Ordering::Acquire),
        )
    })
}

#[test]
fn an_enabled_host_lists_its_directory_without_approval_selection_or_session() {
    // Multi-record (200 machines) and single-record listings, exactly as read.
    for count in [1_u8, 3, 200] {
        let nodes: Vec<_> = (0..count).map(|i| node(i, i == 0)).collect();
        let expected = Listing {
            entries: nodes.iter().map(entry).collect(),
            truncated: count == 3,
        };
        let (host, viewer, notices, alive) = fetch(
            true,
            Some(Ok(Directory {
                nodes,
                truncated: count == 3,
            })),
        );
        assert_eq!(host, Err(Error::InspectionComplete));
        assert_eq!(viewer, Ok(expected));
        assert_eq!(notices, 0);
        assert!(!alive, "the admission was retired");
    }
}

#[test]
fn a_machine_the_wire_cannot_carry_is_dropped_and_the_listing_says_so() {
    let mut outside = node(2, false);
    outside.ipv4 = Some(Ipv4Addr::new(192, 168, 1, 2));
    outside.ipv6 = None;
    let mut unnamed = node(3, false);
    unnamed.name = "Not A Name".into();
    let nodes = vec![
        node(0, true),
        node(1, false),
        outside,
        unnamed,
        node(4, false),
    ];
    let (host, viewer, notices, _) = fetch(
        true,
        Some(Ok(Directory {
            nodes: nodes.clone(),
            truncated: false,
        })),
    );
    assert_eq!(host, Err(Error::InspectionComplete));
    let listing = viewer.unwrap();
    assert!(listing.truncated, "an omission is never silent");
    assert_eq!(
        listing.entries,
        vec![entry(&nodes[0]), entry(&nodes[1]), entry(&nodes[4])]
    );
    assert_eq!(notices, 0);
}

#[test]
fn a_disabled_host_refuses_typed_before_reading_anything() {
    // The fixture would list; the host must not consult it.
    let (host, viewer, notices, alive) = fetch(
        false,
        Some(Ok(Directory {
            nodes: vec![node(0, true), node(1, false)],
            truncated: false,
        })),
    );
    assert_eq!(host, Err(Error::InspectionAbandoned));
    assert_eq!(viewer, Err(refused(Reason::DirectoryDisabled)));
    assert_eq!(notices, 0);
    assert!(!alive);
}

#[test]
fn an_unreadable_or_unlistable_directory_is_refused_as_unavailable() {
    let mut bad_host = node(0, true);
    bad_host.ipv4 = Some(Ipv4Addr::new(10, 0, 0, 1));
    bad_host.ipv6 = None;
    let not_first = Directory {
        nodes: vec![node(1, false), node(0, true)],
        truncated: false,
    };
    for fixture in [
        Some(Err(fr_tailnet::Error::LocalApiUnavailable)),
        None,
        Some(Ok(Directory {
            nodes: vec![],
            truncated: false,
        })),
        Some(Ok(Directory {
            nodes: vec![bad_host, node(1, false)],
            truncated: false,
        })),
        Some(Ok(not_first)),
    ] {
        let (host, viewer, notices, alive) = fetch(true, fixture);
        assert_eq!(host, Err(Error::InspectionAbandoned));
        assert_eq!(viewer, Err(refused(Reason::DirectoryUnavailable)));
        assert_eq!(notices, 0);
        assert!(!alive);
    }
}

#[test]
fn a_directory_viewer_cannot_select_a_session_even_without_approval() {
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
        host.serve_directory().unwrap();
        host.directory_fixture = Some(Ok(Directory {
            nodes: vec![node(0, true)],
            truncated: false,
        }));
        let mut viewer = Viewer::new(
            cx.clone(),
            client.unwrap(),
            directory::prepare_offer(offer()).unwrap(),
            policy,
            Duration::from_secs(2),
        )
        .unwrap();
        let notices = Cell::new(0);
        let host_run = host.open(Duration::from_millis(1), |_, _| {
            notices.set(notices.get() + 1);
            Err(())
        });
        // Ordinary startup despite the required marker: its
        // SelectedConfiguration is not a directory request and is refused.
        let viewer_run = async {
            for _ in 0..1000 {
                if let Err(error) = viewer.drive(Duration::from_millis(1)).await {
                    return error;
                }
                assert!(!viewer.is_complete());
            }
            panic!("directory selection did not terminate");
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
fn the_opt_in_is_only_before_the_hello() {
    let runtime = network::runtime();
    let cx = runtime.request_cx_with_budget(Budget::INFINITE);
    let hcx = runtime.request_cx_with_budget(Budget::INFINITE);
    runtime.block_on(async {
        let (_client, server) = network::native_pair(&cx, "localhost", quic::ALPN).await;
        let mut host = connection_test_host(hcx, server.unwrap(), offer(), quic::Policy::default());
        host.serve_directory().unwrap();
        host.serve_directory().unwrap();
        host.phase = crate::session_startup::Phase::Selection;
        assert_eq!(host.serve_directory(), Err(Error::Order));
    });
}
