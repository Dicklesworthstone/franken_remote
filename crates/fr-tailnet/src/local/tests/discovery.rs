//! Synthetic Tailscale metadata over real credential-checked Unix/HTTP/runtime.
use super::*;
use crate::PeerTransportPath;
use crate::local::tests::{
    peer_mut,
    support::{Server, response, runtime},
};
use asupersync::{cx::Cx, types::CancelKind};
use serde_json::{Value, json};
use std::sync::atomic::AtomicUsize;
fn fixtures() -> (Value, Value) {
    let key = format!("nodekey:{}", "2".repeat(64));
    (
        json!({"Version":"discovery-fixture-not-version-qualification","BackendState":"Running",
        "TailscaleIPs":["100.64.0.1"],"CurrentTailnet":{"Name":"fixture.invalid","MagicDNSSuffix":"fixture.ts.net"},
        "Self":{"ID":"n-host","NodeID":1,"PublicKey":format!("nodekey:{}", "1".repeat(64)),"UserID":7,"TailscaleIPs":["100.64.0.1"],"InNetworkMap":true},
        "Peer":{key.clone():{"ID":"n-peer","NodeID":2,"PublicKey":key,"UserID":7,"TailscaleIPs":["100.64.0.2"],"InNetworkMap":true}}}),
        Value::Null,
    )
}
fn fixture_server(replies: Vec<Vec<u8>>, pause: Duration) -> Server {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-discover-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    Server::status_only(&path, replies, pause, true)
}

fn status() -> Value {
    let (mut s, _) = fixtures();
    s["Self"]["DNSName"] = json!("local.fixture.ts.net.");
    peer_mut(&mut s)["DNSName"] = json!("remote.fixture.ts.net.");
    s
}
#[test]
fn discovery_reads_once_and_reports_machines_not_desktop_capabilities() {
    let server = fixture_server(vec![response(&status(), true)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let before = now(&cx).unwrap();
        let discovery = server.client.discover(&cx).await.unwrap();
        assert!(discovery.lookup_started_us() >= before);
        assert!(discovery.lookup_completed_us() >= discovery.lookup_started_us());
        assert_eq!(discovery.excluded(), crate::DiscoveryExclusions::default());
        assert_eq!(discovery.peers().len(), 1);
        let peer = &discovery.peers()[0];
        assert_eq!(peer.stable_id(), "n-peer");
        assert_eq!(peer.certificate_name(), "remote.fixture.ts.net");
        assert_eq!(
            peer.addresses(),
            &["100.64.0.2".parse::<std::net::IpAddr>().unwrap()]
        );
        let debug = format!("{discovery:?} {peer:?}");
        for private in ["n-peer", "fixture", "100.64", "nodekey"] {
            assert!(!debug.contains(private));
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
        // Discovery did not consume an authority slot. Fresh resolution still
        // performs its own TWO reads and produces its separate expiring target.
        let target = server
            .client
            .peer_target(&cx, crate::PeerSelector::StableId(peer.stable_id()))
            .await
            .unwrap();
        assert_eq!(target.stable_id(), "n-peer");
        assert_eq!(server.calls.load(Ordering::SeqCst), 3);
    });
}
#[test]
fn discovery_never_guesses_shared_expired_ambiguous_or_subnet_destinations() {
    for (field, value, expected) in [
        (
            "ShareeNode",
            json!(true),
            crate::DiscoveryExclusions {
                shared: 1,
                ..Default::default()
            },
        ),
        (
            "AltSharerUserID",
            json!(19),
            crate::DiscoveryExclusions {
                shared: 1,
                ..Default::default()
            },
        ),
        (
            "Expired",
            json!(true),
            crate::DiscoveryExclusions {
                expired: 1,
                ..Default::default()
            },
        ),
        (
            "KeyExpiry",
            json!("2020-01-01T00:00:00Z"),
            crate::DiscoveryExclusions {
                expired: 1,
                ..Default::default()
            },
        ),
        (
            "InNetworkMap",
            json!(false),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
        (
            "DNSName",
            json!("remote.attacker.invalid"),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
        (
            "TailscaleIPs",
            json!(["100.64.0.1"]),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
        (
            "TailscaleIPs",
            json!(["::ffff:100.64.0.2"]),
            crate::DiscoveryExclusions {
                unusable: 1,
                ..Default::default()
            },
        ),
    ] {
        let mut s = status();
        peer_mut(&mut s)[field] = value;
        let server = fixture_server(vec![response(&s, false)], Duration::ZERO);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let discovery = server.client.discover(&cx).await.unwrap();
            assert!(discovery.peers().is_empty(), "{field}");
            assert_eq!(discovery.excluded(), expected, "{field}");
        });
    }
}
#[test]
fn discovery_is_stably_ordered_and_conflicting_identities_are_not_arbitrarily_selected() {
    let mut s = status();
    let key = format!("nodekey:{}", "3".repeat(64));
    let mut other = peer_mut(&mut s).clone();
    other["ID"] = json!("a-first");
    other["NodeID"] = json!(3);
    other["PublicKey"] = json!(key);
    other["TailscaleIPs"] = json!(["100.64.0.3"]);
    other["DNSName"] = json!("other.fixture.ts.net.");
    s["Peer"][&key] = other;
    let server = fixture_server(vec![response(&s, false)], Duration::ZERO);
    runtime().block_on(async {
        let d = server
            .client
            .discover(&Cx::current().unwrap())
            .await
            .unwrap();
        assert_eq!(
            d.peers()
                .iter()
                .map(crate::DiscoveredPeer::stable_id)
                .collect::<Vec<_>>(),
            ["a-first", "n-peer"]
        );
    });
    s["Peer"][&key]["TailscaleIPs"] = json!(["100.64.0.2"]);
    let server = fixture_server(vec![response(&s, false)], Duration::ZERO);
    runtime().block_on(async {
        let d = server
            .client
            .discover(&Cx::current().unwrap())
            .await
            .unwrap();
        assert!(d.peers().is_empty());
        assert_eq!(d.excluded().unusable, 2);
    });
}
#[test]
fn failed_host_identity_refuses_all_discovery_instead_of_returning_an_empty_tailnet() {
    for (field, value, expected) in [
        ("DNSName", json!("localhost"), Error::MalformedMetadata),
        ("Expired", json!(true), Error::KeyExpired),
        (
            "KeyExpiry",
            json!("2020-01-01T00:00:00Z"),
            Error::KeyExpired,
        ),
    ] {
        let mut s = status();
        s["Self"][field] = value;
        let server = fixture_server(vec![response(&s, false)], Duration::ZERO);
        runtime().block_on(async {
            assert_eq!(
                server
                    .client
                    .discover(&Cx::current().unwrap())
                    .await
                    .unwrap_err(),
                expected
            );
        });
    }
}
#[test]
fn discovery_respects_cancellation_busy_and_kernel_peer_credentials() {
    let mut server = fixture_server(vec![response(&status(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        server.client.busy.store(true, Ordering::Release);
        assert_eq!(server.client.discover(&cx).await.unwrap_err(), Error::Busy);
        assert_eq!(server.calls.load(Ordering::SeqCst), 0);
        server.client.busy.store(false, Ordering::Release);
        server.client.daemon_uid = server.client.daemon_uid.wrapping_add(1);
        assert_eq!(
            server.client.discover(&cx).await.unwrap_err(),
            Error::UntrustedLocalApi
        );
        assert_eq!(server.calls.load(Ordering::SeqCst), 0);
        assert!(!server.client.busy.load(Ordering::Acquire));
        cx.cancel_fast(CancelKind::User);
        assert_eq!(
            server.client.discover(&cx).await.unwrap_err(),
            Error::Cancelled
        );
    });
}
#[test]
fn stalled_discovery_has_a_fixed_deadline_and_releases_the_lookup_slot() {
    let server = fixture_server(
        vec![response(&status(), false)],
        Duration::from_millis(1200),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        assert_eq!(
            server.client.discover(&cx).await.unwrap_err(),
            Error::Timeout
        );
        assert!(!server.client.busy.load(Ordering::Acquire));
        assert_eq!(server.calls.load(Ordering::SeqCst), 1);
    });
}
#[test]
fn malformed_or_over_limit_snapshots_refuse_instead_of_partially_discovering() {
    let mut s = status();
    // Parser bound is checked before any candidate is returned.
    let peer = peer_mut(&mut s).clone();
    s["Peer"] = Value::Object(
        (0..1025)
            .map(|i| (format!("{i:080}"), peer.clone()))
            .collect(),
    );
    let server = fixture_server(vec![response(&s, false)], Duration::ZERO);
    runtime().block_on(async {
        assert_eq!(
            server
                .client
                .discover(&Cx::current().unwrap())
                .await
                .unwrap_err(),
            Error::MalformedMetadata
        );
    });
}
/// `Relay` is the home DERP region and is present for nearly every peer, so it
/// never implies relaying on its own. Live evidence (2026-10-12): a LAN peer
/// with `CurAddr` 192.168.1.124:41641 and `Relay` "nyc" was reported relayed.
#[test]
fn discovery_reports_transport_path_direct_relayed_and_idle() {
    let cases = [
        // Direct endpoint in use; the home region is irrelevant.
        (
            Some("192.168.1.50:41641"),
            Some("nyc"),
            true,
            PeerTransportPath::Direct {
                cur_addr: "192.168.1.50:41641".to_string(),
            },
        ),
        (
            Some("192.168.1.50:41641"),
            None,
            false,
            PeerTransportPath::Direct {
                cur_addr: "192.168.1.50:41641".to_string(),
            },
        ),
        // Recent traffic without a direct endpoint: relayed through DERP.
        (
            Some(""),
            Some("nyc"),
            true,
            PeerTransportPath::DerpRelayed {
                relay: "nyc".to_string(),
            },
        ),
        (
            None,
            Some("nyc"),
            true,
            PeerTransportPath::DerpRelayed {
                relay: "nyc".to_string(),
            },
        ),
        // Idle: a home region alone is not a path.
        (Some(""), Some("nyc"), false, PeerTransportPath::Unknown),
        (None, Some("nyc"), false, PeerTransportPath::Unknown),
        (None, Some(""), true, PeerTransportPath::Unknown),
        (None, None, true, PeerTransportPath::Unknown),
    ];
    for (cur_addr, relay, active, expected) in cases {
        let mut s = status();
        if let Some(cur_addr) = cur_addr {
            peer_mut(&mut s)["CurAddr"] = json!(cur_addr);
        }
        if let Some(relay) = relay {
            peer_mut(&mut s)["Relay"] = json!(relay);
        }
        peer_mut(&mut s)["Active"] = json!(active);
        let server = fixture_server(vec![response(&s, false)], Duration::ZERO);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let discovery = server.client.discover(&cx).await.unwrap();
            assert_eq!(discovery.peers().len(), 1);
            assert_eq!(
                discovery.peers()[0].transport_path(),
                &expected,
                "{cur_addr:?} {relay:?} active={active}"
            );
        });
    }
}
