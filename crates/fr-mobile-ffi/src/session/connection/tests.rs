use super::*;
use fr_tailnet::{DiscoveredPeer, DiscoveryExclusions};

fn addresses() -> Vec<IpAddr> {
    vec![
        "100.64.0.2".parse().unwrap(),
        "fd7a:115c:a1e0::2".parse().unwrap(),
    ]
}

fn peer(id: &str, name: &str) -> DiscoveredPeer {
    DiscoveredPeer::from_parts(
        id.to_owned(),
        name.to_owned(),
        addresses(),
        PeerTransportPath::Unknown,
    )
}

fn discovery(peers: Vec<DiscoveredPeer>) -> Discovery {
    Discovery::from_parts(peers, DiscoveryExclusions::default(), 1, 2)
}

fn snapshot() -> Discovery {
    discovery(vec![peer("n-selected", "remote.fixture.ts.net")])
}

#[test]
fn exact_node_id_and_normalized_name_retain_the_same_identity() {
    for selection in [
        "n-selected",
        "remote.fixture.ts.net",
        "REMOTE.FIXTURE.TS.NET.",
    ] {
        let target =
            Target::installed(&snapshot(), selection, Vec::new(), AddressFamily::Automatic)
                .unwrap();
        let Some(PeerSelector::Pinned {
            stable_id,
            certificate_name,
        }) = target.selector()
        else {
            panic!("an installed target must use a pinned fresh lookup")
        };
        assert_eq!(stable_id, "n-selected");
        assert_eq!(certificate_name, "remote.fixture.ts.net");
        assert_eq!(target.certificate_name(), certificate_name);
        assert_eq!(target.family(), AddressFamily::Automatic);
    }
}

#[test]
fn selection_is_owned_and_a_new_snapshot_cannot_retarget_it() {
    let original = snapshot();
    let retained =
        Target::installed(&original, "n-selected", Vec::new(), AddressFamily::Ipv6).unwrap();
    drop(original);
    let renamed = discovery(vec![peer("n-selected", "renamed.fixture.ts.net")]);
    let replacement = discovery(vec![peer("n-replacement", "remote.fixture.ts.net")]);
    assert!(
        Target::installed(
            &renamed,
            "remote.fixture.ts.net",
            Vec::new(),
            AddressFamily::Ipv6
        )
        .is_err()
    );
    assert!(
        Target::installed(&replacement, "n-selected", Vec::new(), AddressFamily::Ipv6).is_err()
    );
    let copied = retained.clone();
    let Some(PeerSelector::Pinned {
        stable_id,
        certificate_name,
    }) = copied.selector()
    else {
        panic!("the original installed identity must be retained")
    };
    assert_eq!(stable_id, "n-selected");
    assert_eq!(certificate_name, "remote.fixture.ts.net");
    assert_eq!(copied.family(), AddressFamily::Ipv6);
}

#[test]
fn missing_short_invalid_and_ambiguous_choices_never_fall_back_to_dns() {
    for selection in [
        "",
        "remote",
        "n-SELECTED",
        "unknown.fixture.ts.net",
        "remote.fixture.ts.net..",
        "https://remote.fixture.ts.net",
    ] {
        assert_eq!(
            Target::installed(&snapshot(), selection, Vec::new(), AddressFamily::Automatic)
                .unwrap_err(),
            Error::IdentityMismatch
        );
    }
    let ambiguous = discovery(vec![
        peer("remote.fixture.ts.net", "first.fixture.ts.net"),
        peer("n-remote", "remote.fixture.ts.net"),
    ]);
    assert_eq!(
        Target::installed(
            &ambiguous,
            "remote.fixture.ts.net",
            Vec::new(),
            AddressFamily::Automatic
        )
        .unwrap_err(),
        Error::IdentityMismatch
    );
}

#[test]
fn duplicate_and_malformed_discovery_rows_refuse_the_whole_snapshot() {
    for rows in [
        vec![
            peer("n-same", "first.fixture.ts.net"),
            peer("n-same", "second.fixture.ts.net"),
        ],
        vec![
            peer("n-first", "same.fixture.ts.net"),
            peer("n-second", "same.fixture.ts.net"),
        ],
        vec![peer("invalid node", "remote.fixture.ts.net")],
        vec![peer("n-selected", "REMOTE.fixture.ts.net")],
    ] {
        assert_eq!(
            Target::installed(
                &discovery(rows),
                "remote.fixture.ts.net",
                Vec::new(),
                AddressFamily::Automatic
            )
            .unwrap_err(),
            Error::MalformedMetadata
        );
    }
    let oversized = discovery(
        (0..=MAX_CANDIDATES)
            .map(|i| peer(&format!("n-{i}"), &format!("host-{i}.fixture.ts.net")))
            .collect(),
    );
    assert_eq!(
        Target::installed(&oversized, "n-0", Vec::new(), AddressFamily::Automatic).unwrap_err(),
        Error::MalformedMetadata
    );
}

#[test]
fn exact_addresses_remain_constraints_on_the_original_pinned_identity() {
    let mut explicit = addresses();
    explicit.reverse();
    let target = Target::installed(
        &snapshot(),
        "n-selected",
        explicit.clone(),
        AddressFamily::Ipv4,
    )
    .unwrap();
    let Some(PeerSelector::PinnedAddresses {
        stable_id,
        certificate_name,
        addresses: constrained,
    }) = target.selector()
    else {
        panic!("explicit addresses must reach fresh metadata validation")
    };
    assert_eq!(stable_id, "n-selected");
    assert_eq!(certificate_name, "remote.fixture.ts.net");
    assert_eq!(constrained, explicit);
    assert_eq!(target.family(), AddressFamily::Ipv4);
}

#[test]
fn partial_foreign_duplicate_and_oversized_address_constraints_refuse() {
    let known = addresses();
    for explicit in [
        vec![known[0]],
        vec![known[1]],
        vec![known[0], known[0]],
        vec![known[0], "fd7a:115c:a1e0::3".parse().unwrap()],
        vec![known[0], known[1], "100.64.0.3".parse().unwrap()],
    ] {
        assert_eq!(
            Target::installed(
                &snapshot(),
                "n-selected",
                explicit,
                AddressFamily::Automatic
            )
            .unwrap_err(),
            Error::AddressMismatch
        );
    }
    assert_eq!(
        Target::installed(
            &snapshot(),
            "n-selected",
            vec![known[0]; 9],
            AddressFamily::Automatic
        )
        .unwrap_err(),
        Error::InvalidEndpoint
    );
}

#[test]
fn named_directory_routes_keep_the_exact_name_and_pin_requirement() {
    let host = HostName::parse("remote.fixture.ts.net").unwrap();
    let pin = HostPin::from_addresses(host.clone(), addresses()).unwrap();
    let other = HostName::parse("other.fixture.ts.net").unwrap();
    assert_eq!(
        Target::named(other, Some(pin.clone()), false).unwrap_err(),
        Error::InvalidEndpoint
    );
    assert_eq!(
        Target::named(host.clone(), None, true).unwrap_err(),
        Error::InvalidEndpoint
    );
    for target in [
        Target::named(host.clone(), None, false).unwrap(),
        Target::named(host.clone(), Some(pin.clone()), false).unwrap(),
        Target::named(host, Some(pin), true).unwrap(),
    ] {
        assert_eq!(target.certificate_name(), "remote.fixture.ts.net");
        assert!(target.selector().is_none());
        assert_eq!(target.family(), AddressFamily::Automatic);
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn installed_and_named_targets_construct_only_their_matching_client_routes() {
    let installed = Target::installed(
        &snapshot(),
        "n-selected",
        Vec::new(),
        AddressFamily::Automatic,
    )
    .unwrap();
    assert!(installed.client().unwrap().pin_watch().is_none());
    let named = Target::named(
        HostName::parse("remote.fixture.ts.net").unwrap(),
        None,
        false,
    )
    .unwrap();
    assert!(named.client().unwrap().pin_watch().is_some());
}

#[test]
fn diagnostics_do_not_reveal_the_selected_node_name_or_addresses() {
    let target = Target::installed(
        &snapshot(),
        "n-selected",
        addresses(),
        AddressFamily::Automatic,
    )
    .unwrap();
    let diagnostic = format!("{target:?} {:?}", target.selector());
    for content in ["n-selected", "fixture", "100.64.0.2", "fd7a:115c:a1e0"] {
        assert!(!diagnostic.contains(content));
    }
}
