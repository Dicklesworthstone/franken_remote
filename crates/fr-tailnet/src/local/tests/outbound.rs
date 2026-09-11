//! Identity metadata is synthetic; the Unix/HTTP adapter and clocks are real.
use super::*;
use crate::PeerSelector;
fn status() -> Value {
    let (mut s, _) = fixtures();
    s["Self"]["DNSName"] = json!("local.fixture.ts.net.");
    peer_mut(&mut s)["DNSName"] = json!("remote.fixture.ts.net.");
    s
}
#[test]
fn resolves_only_node_owned_targets_with_canonical_tls_name() {
    let s = status();
    let server = Server::new(vec![response(&s, true)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for selector in [
            PeerSelector::StableId("n-peer"),
            PeerSelector::Name("REMOTE.fixture.ts.net."),
            PeerSelector::Address("100.64.0.2".parse().unwrap()),
        ] {
            let before = now(&cx).unwrap();
            let target = server.client.peer_target(&cx, selector).await.unwrap();
            assert_eq!(target.certificate_name(), "remote.fixture.ts.net");
            assert_eq!(
                target.addresses(),
                &["100.64.0.2".parse::<std::net::IpAddr>().unwrap()]
            );
            assert_eq!(
                target.local_addresses(),
                &["100.64.0.1".parse::<std::net::IpAddr>().unwrap()]
            );
            assert!(target.issued_us() >= before);
            assert_eq!(target.expires_us(), target.issued_us() + 3_000_000);
            assert!(!format!("{target:?} {selector:?}").contains("fixture"));
            assert!(!format!("{target:?}").contains("100.64"));
        }
    });
    assert_eq!(server.calls.load(Ordering::SeqCst), 6);
}
#[test]
fn invalid_local_selectors_do_not_send_requests_and_no_dns_fallback_exists() {
    let server = Server::new(vec![response(&status(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for selector in [
            PeerSelector::Name("https://remote.fixture.ts.net"),
            PeerSelector::Name("x\r\nHost:y"),
            PeerSelector::StableId(""),
            PeerSelector::Address("127.0.0.1".parse().unwrap()),
            PeerSelector::Address("::ffff:100.64.0.2".parse().unwrap()),
        ] {
            assert!(matches!(
                server.client.peer_target(&cx, selector).await,
                Err(Error::InvalidEndpoint)
            ));
        }
        assert_eq!(server.calls.load(Ordering::SeqCst), 0);
        for selector in [
            PeerSelector::Name("remote"),
            PeerSelector::Address("192.168.0.1".parse().unwrap()),
            PeerSelector::StableId("n-host"),
        ] {
            assert!(matches!(
                server.client.peer_target(&cx, selector).await,
                Err(Error::IdentityMismatch)
            ));
        }
    });
}
#[test]
fn target_refresh_uses_original_origin_and_never_revives_expired_metadata() {
    let server = Server::new(vec![response(&status(), false)], Duration::ZERO);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let target = server
            .client
            .peer_target(&cx, PeerSelector::StableId("n-peer"))
            .await
            .unwrap();
        let next = server
            .client
            .revalidate_peer_target(&cx, &target)
            .await
            .unwrap();
        assert!(target.same_identity(&next));
        let foreign = LocalApi::new(&*server.client.path).unwrap();
        assert_eq!(
            foreign.check_peer_target(&cx, &target),
            Err(Error::IdentityMismatch)
        );
        let mut old = next;
        old.expires_us = now(&cx).unwrap();
        let calls = server.calls.load(Ordering::SeqCst);
        assert!(matches!(
            server.client.revalidate_peer_target(&cx, &old).await,
            Err(Error::Expired)
        ));
        assert_eq!(server.calls.load(Ordering::SeqCst), calls);
    });
}
#[test]
fn changed_target_address_key_name_or_local_tailnet_never_refreshes_old_connection() {
    for field in ["address", "key", "name", "tailnet"] {
        let a = status();
        let mut b = a.clone();
        match field {
            "address" => peer_mut(&mut b)["TailscaleIPs"] = json!(["100.64.0.3"]),
            "key" => {
                let key = format!("nodekey:{}", "8".repeat(64));
                let mut peer = peer_mut(&mut b).clone();
                peer["PublicKey"] = json!(key);
                b["Peer"] = json!({key:peer});
            }
            "name" => peer_mut(&mut b)["DNSName"] = json!("replacement.fixture.ts.net."),
            _ => b["CurrentTailnet"]["Name"] = json!("new-tailnet"),
        }
        let server = Server::new(
            vec![
                response(&a, false),
                response(&a, false),
                response(&b, false),
            ],
            Duration::ZERO,
        );
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let target = server
                .client
                .peer_target(&cx, PeerSelector::StableId("n-peer"))
                .await
                .unwrap();
            assert!(matches!(
                server.client.revalidate_peer_target(&cx, &target).await,
                Err(Error::IdentityChanged)
            ));
        });
    }
}
#[test]
fn conflicting_or_shared_or_expired_nodes_never_select_a_destination() {
    for mode in 0..9 {
        let mut s = status();
        match mode {
            0 => peer_mut(&mut s)["ShareeNode"] = json!(true),
            1 => peer_mut(&mut s)["AltSharerUserID"] = json!(12),
            2 => peer_mut(&mut s)["Expired"] = json!(true),
            3 => peer_mut(&mut s)["KeyExpiry"] = json!("2020-01-01T00:00:00Z"),
            4 => peer_mut(&mut s)["DNSName"] = json!("remote.evil.invalid."),
            5 => peer_mut(&mut s)["InNetworkMap"] = json!(false),
            6 => peer_mut(&mut s)["TailscaleIPs"] = json!(["100.64.0.1"]),
            7 => peer_mut(&mut s)["TailscaleIPs"] = json!(["::ffff:100.64.0.2"]),
            _ => {
                let mut other = peer_mut(&mut s).clone();
                other["ID"] = json!("n-other");
                other["NodeID"] = json!(3);
                let key = format!("nodekey:{}", "3".repeat(64));
                other["PublicKey"] = json!(key);
                other["DNSName"] = json!("other.fixture.ts.net.");
                s["Peer"][key] = other;
            }
        }
        let server = Server::new(vec![response(&s, false)], Duration::ZERO);
        runtime().block_on(async {
            assert!(
                server
                    .client
                    .peer_target(&Cx::current().unwrap(), PeerSelector::StableId("n-peer"))
                    .await
                    .is_err()
            );
        });
    }
}
#[test]
fn unrelated_peer_changes_do_not_replace_the_selected_destination() {
    let a = status();
    let mut b = a.clone();
    let key = format!("nodekey:{}", "3".repeat(64));
    let mut other = peer_mut(&mut b).clone();
    other["PublicKey"] = json!(key);
    other["ID"] = json!("n-third");
    other["NodeID"] = json!(3);
    other["DNSName"] = json!("third.fixture.ts.net.");
    other["TailscaleIPs"] = json!(["100.64.0.3"]);
    b["Peer"][key] = other;
    let server = Server::new(
        vec![response(&a, false), response(&b, false)],
        Duration::ZERO,
    );
    runtime().block_on(async {
        server
            .client
            .peer_target(&Cx::current().unwrap(), PeerSelector::StableId("n-peer"))
            .await
            .unwrap();
    });
}
#[test]
fn inconsistent_snapshot_and_slow_lookup_cannot_mint_fresh_target() {
    let a = status();
    let mut b = a.clone();
    peer_mut(&mut b)["UserID"] = json!(123);
    let server = Server::new(
        vec![response(&a, false), response(&b, false)],
        Duration::ZERO,
    );
    runtime().block_on(async {
        assert!(matches!(
            server
                .client
                .peer_target(&Cx::current().unwrap(), PeerSelector::StableId("n-peer"))
                .await,
            Err(Error::SnapshotChanged)
        ));
    });
    let server = Server::new(vec![response(&a, false)], Duration::from_millis(550));
    runtime().block_on(async {
        assert!(matches!(
            server
                .client
                .peer_target(&Cx::current().unwrap(), PeerSelector::StableId("n-peer"))
                .await,
            Err(Error::Timeout)
        ));
        assert!(!server.client.busy.load(Ordering::Acquire));
    });
}
