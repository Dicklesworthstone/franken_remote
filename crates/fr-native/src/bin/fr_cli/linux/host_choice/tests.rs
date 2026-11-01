use super::*;
use fr_tailnet::{DiscoveredPeer, Discovery, DiscoveryExclusions, PeerSelector, PeerTransportPath};

fn peer(id: &str, name: &str, path: PeerTransportPath) -> DiscoveredPeer {
    DiscoveredPeer::from_parts(
        id.into(),
        name.into(),
        vec!["100.80.0.1".parse().unwrap()],
        path,
    )
}
fn snapshot(peers: Vec<DiscoveredPeer>) -> Discovery {
    Discovery::from_parts(peers, DiscoveryExclusions::default(), 100, 200)
}

#[test]
fn native_ordinals_keep_the_sorted_original_id_name_and_explicit_port() {
    let original = snapshot(vec![
        peer("n-later", "zulu.fixture.ts.net", PeerTransportPath::Unknown),
        peer(
            "n-first",
            "alpha.fixture.ts.net",
            PeerTransportPath::Direct {
                cur_addr: "192.0.2.90:41641".into(),
            },
        ),
    ]);
    let choices = catalog(&original).unwrap();
    assert_eq!(rows(&choices).unwrap().len(), 2);
    let selected = Selection::from_candidate(choices.get(0).unwrap(), 9443);
    assert_eq!(selected.port, 9443);
    assert!(matches!(
        selected.selector(),
        PeerSelector::Pinned {
            stable_id: "n-first",
            certificate_name: "alpha.fixture.ts.net"
        }
    ));
    // A replacement snapshot cannot retarget the original chooser ordinal.
    let replacement = catalog(&snapshot(vec![peer(
        "n-replacement",
        "alpha.fixture.ts.net",
        PeerTransportPath::Unknown,
    )]))
    .unwrap();
    assert_ne!(choices.get(0).unwrap(), replacement.get(0).unwrap());
    assert!(matches!(
        selected.selector(),
        PeerSelector::Pinned {
            stable_id: "n-first",
            certificate_name: "alpha.fixture.ts.net"
        }
    ));
}

#[test]
fn malformed_or_ambiguous_discovery_never_produces_a_native_choice() {
    for peers in [
        vec![peer("n-one", "bad\nname", PeerTransportPath::Unknown)],
        vec![
            peer("n-one", "one.fixture.ts.net", PeerTransportPath::Unknown),
            peer("n-two", "one.fixture.ts.net", PeerTransportPath::Unknown),
        ],
        vec![
            peer("n-one", "one.fixture.ts.net", PeerTransportPath::Unknown),
            peer("n-one", "two.fixture.ts.net", PeerTransportPath::Unknown),
        ],
    ] {
        assert_eq!(
            catalog(&snapshot(peers)).unwrap_err().code,
            "host_choice_invalid_snapshot"
        );
    }
    let empty = catalog(&snapshot(vec![])).unwrap();
    assert!(empty.is_empty() && empty.get(0).is_err());
}

#[test]
fn selection_report_is_identity_metadata_without_a_readiness_or_authority_claim() {
    let choice = Candidate::new(
        "n-first".into(),
        "alpha.fixture.ts.net".into(),
        PathHint::Relayed,
    )
    .unwrap();
    let report: serde_json::Value =
        serde_json::from_str(&selected_report(&choice, 9443, true)).unwrap();
    assert_eq!(report["event"], "host_selected");
    assert_eq!(report["node_id"], "n-first");
    assert_eq!(report["certificate_name"], "alpha.fixture.ts.net");
    assert_eq!(report["port"], 9443);
    assert_eq!(report["selection_only"], true);
    assert_eq!(report["fresh_lookup_required"], true);
    assert_eq!(report["remote_session_started"], false);
    assert_eq!(report["input_requested"], false);
    for absent in [
        "addresses",
        "online",
        "authorized",
        "lease",
        "desktop_ready",
    ] {
        assert!(report.get(absent).is_none());
    }
}
