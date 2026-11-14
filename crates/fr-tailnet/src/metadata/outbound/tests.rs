//! Sanitized metadata fixtures, never evidence of a live macOS installation.
use super::*;
use crate::metadata::WhoIs;
use serde_json::{Value, json};
use std::net::IpAddr;

fn fixtures() -> (Value, Value) {
    let mut status: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/linux-1.102.3/status.json"
    ))
    .unwrap();
    let who: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/linux-1.102.3/whois.json"
    ))
    .unwrap();
    status["Self"]["DNSName"] = json!("local.fixture.ts.net.");
    peer(&mut status)["DNSName"] = json!("remote.fixture.ts.net.");
    (status, who)
}
fn peer(status: &mut Value) -> &mut Value {
    status["Peer"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap()
}
fn target(status: &Value) -> Target {
    resolve(
        &Status::parse(&serde_json::to_vec(status).unwrap()).unwrap(),
        PeerSelector::StableId("n-2"),
    )
    .unwrap()
}
fn verify(status: &Value, who: &Value, ip: &str) -> Result<Option<String>, Error> {
    verify_whois(
        &target(status),
        ip.parse::<IpAddr>().unwrap(),
        &WhoIs::parse(&serde_json::to_vec(who).unwrap())?,
    )
}

fn pinned_addresses(status: &Value, addresses: &[IpAddr]) -> Result<Target, Error> {
    resolve(
        &Status::parse(&serde_json::to_vec(status).unwrap()).unwrap(),
        PeerSelector::PinnedAddresses {
            stable_id: "n-2",
            certificate_name: "remote.fixture.ts.net",
            addresses,
        },
    )
}

#[test]
fn explicit_addresses_constrain_the_full_fresh_set_without_replacing_metadata() {
    let (status, _) = fixtures();
    let addresses = [
        "fd7a:115c:a1e0::2".parse().unwrap(),
        "100.64.0.2".parse().unwrap(),
    ];
    let selected = pinned_addresses(&status, &addresses).unwrap();
    let ordinary = target(&status);
    assert!(selected == ordinary);
    assert_eq!(selected.certificate_name, "remote.fixture.ts.net");
    assert_eq!(selected.peer.ips.0, ordinary.peer.ips.0);
    assert_eq!(
        format!(
            "{:?}",
            PeerSelector::PinnedAddresses {
                stable_id: "n-2",
                certificate_name: "remote.fixture.ts.net",
                addresses: &addresses,
            }
        ),
        "PeerSelector([redacted])"
    );
}

#[test]
fn partial_foreign_and_extra_explicit_addresses_never_select_a_peer() {
    let (status, _) = fixtures();
    for addresses in [
        vec!["100.64.0.2"],
        vec!["fd7a:115c:a1e0::2"],
        vec!["100.64.0.3", "fd7a:115c:a1e0::2"],
        vec!["100.64.0.2", "fd7a:115c:a1e0::3"],
        vec!["100.64.0.2", "fd7a:115c:a1e0::2", "100.64.0.3"],
    ] {
        let addresses: Vec<IpAddr> = addresses.iter().map(|ip| ip.parse().unwrap()).collect();
        assert_eq!(
            pinned_addresses(&status, &addresses).err(),
            Some(Error::AddressMismatch)
        );
    }
}

#[test]
fn explicit_address_selection_rejects_empty_duplicate_and_oversized_sets() {
    let (status, _) = fixtures();
    let v4: IpAddr = "100.64.0.2".parse().unwrap();
    let v6: IpAddr = "fd7a:115c:a1e0::2".parse().unwrap();
    for addresses in [
        vec![],
        vec![v4, v4],
        vec![v4, v6, v6],
        (2..=10)
            .map(|last| IpAddr::from([100, 64, 0, last]))
            .collect(),
        vec!["::ffff:100.64.0.2".parse().unwrap(), v6],
        vec!["127.0.0.1".parse().unwrap(), v6],
    ] {
        assert_eq!(
            pinned_addresses(&status, &addresses).err(),
            Some(Error::InvalidEndpoint)
        );
    }
}

#[test]
fn explicit_addresses_do_not_authorize_a_renamed_or_replaced_node() {
    let (status, _) = fixtures();
    let addresses = [
        "100.64.0.2".parse().unwrap(),
        "fd7a:115c:a1e0::2".parse().unwrap(),
    ];
    let mut renamed = status.clone();
    peer(&mut renamed)["DNSName"] = json!("renamed.fixture.ts.net.");
    assert_eq!(
        pinned_addresses(&renamed, &addresses).err(),
        Some(Error::IdentityChanged)
    );
    let mut replaced = status;
    peer(&mut replaced)["ID"] = json!("n-replacement");
    assert_eq!(
        pinned_addresses(&replaced, &addresses).err(),
        Some(Error::IdentityMismatch)
    );
}

#[test]
fn exact_whois_identity_keeps_both_families_and_its_key_expiry() {
    let (status, who) = fixtures();
    for ip in ["100.64.0.2", "fd7a:115c:a1e0::2"] {
        assert_eq!(
            verify(&status, &who, ip),
            Ok(Some("2027-02-19T01:13:05Z".into()))
        );
    }
    let mut reordered = who;
    reordered["Node"]["Addresses"] = json!(["fd7a:115c:a1e0::2/128", "100.64.0.2/32"]);
    assert!(verify(&status, &reordered, "100.64.0.2").is_ok());
}

#[test]
fn whois_cannot_replace_a_selected_stable_id_key_user_or_tag() {
    let (status, who) = fixtures();
    for (field, value) in [
        ("ID", json!(3)),
        ("StableID", json!("n-other")),
        ("Key", json!(format!("nodekey:{}", "3".repeat(64)))),
        ("User", json!(8)),
        ("Tags", json!(["tag:server"])),
    ] {
        let mut changed = who.clone();
        changed["Node"][field] = value;
        assert_eq!(
            verify(&status, &changed, "100.64.0.2"),
            Err(Error::IdentityMismatch),
            "{field}"
        );
    }
}

#[test]
fn routed_missing_duplicate_extra_and_foreign_addresses_never_pass() {
    let (status, who) = fixtures();
    for addresses in [
        json!(["100.64.0.2/24", "fd7a:115c:a1e0::2/128"]),
        json!(["100.64.0.2/32", "fd7a:115c:a1e0::2/64"]),
        json!(["100.64.0.2/32"]),
        json!(["100.64.0.2/32", "100.64.0.2/32"]),
        json!(["100.64.0.3/32", "fd7a:115c:a1e0::2/128"]),
        json!(["100.64.0.2/32", "fd7a:115c:a1e0::2/128", "192.168.1.1/32"]),
        json!(["::ffff:100.64.0.2/128", "fd7a:115c:a1e0::2/128"]),
    ] {
        let mut changed = who.clone();
        changed["Node"]["Addresses"] = addresses;
        assert_eq!(
            verify(&status, &changed, "100.64.0.2"),
            Err(Error::AddressMismatch)
        );
    }
    for address in ["100.64.0.1", "100.64.0.3", "192.168.1.1", "::1"] {
        assert_eq!(verify(&status, &who, address), Err(Error::AddressMismatch));
    }
}

#[test]
fn positive_refusals_are_preserved_even_with_matching_status() {
    let (status, who) = fixtures();
    for (field, value, expected) in [
        ("Expired", json!(true), Error::KeyExpired),
        ("Sharer", json!(8), Error::SharedPeer),
        (
            "MachineAuthorized",
            json!(false),
            Error::MachineNotAuthorized,
        ),
        ("IsJailed", json!(true), Error::CapabilityDenied),
        ("UnsignedPeerAPIOnly", json!(true), Error::CapabilityDenied),
    ] {
        let mut changed = who.clone();
        changed["Node"][field] = value;
        assert_eq!(verify(&status, &changed, "100.64.0.2"), Err(expected));
    }
}

#[test]
fn outbound_identity_never_guesses_the_remote_hosts_sharing_scope() {
    let (status, who) = fixtures();
    for tagged in [false, true] {
        let mut status = status.clone();
        let mut who = who.clone();
        peer(&mut status)["UserID"] = json!(8);
        who["Node"]["User"] = json!(8);
        if tagged {
            peer(&mut status)["Tags"] = json!(["tag:server"]);
            who["Node"]["Tags"] = json!(["tag:server"]);
        }
        // Node identity may be selected, but the host still needs its own
        // independent admission, sharing policy and observation/control gates.
        assert!(verify(&status, &who, "100.64.0.2").is_ok());
    }
}

#[test]
fn malformed_oversized_and_duplicate_whois_fields_refuse() {
    let (status, mut who) = fixtures();
    for addresses in [json!(["100.64.0.2"]), json!(["bad/32"])] {
        who["Node"]["Addresses"] = addresses;
        assert_eq!(
            verify(&status, &who, "100.64.0.2"),
            Err(Error::MalformedMetadata)
        );
    }
    let target = target(&status);
    for bytes in [
        vec![b' '; super::super::WHOIS_BYTES + 1],
        b"{\"Node\":{},\"Node\":{}}".to_vec(),
        b"{\"Node\":{\"ID\":2,\"ID\":3}}".to_vec(),
    ] {
        let result = WhoIs::parse(&bytes)
            .and_then(|who| verify_whois(&target, "100.64.0.2".parse().unwrap(), &who));
        assert_eq!(result, Err(Error::MalformedMetadata));
    }
}
