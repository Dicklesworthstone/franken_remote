//! Status JSON fixtures shaped like the installed daemon's
//! `/localapi/v0/status?peers=true`, with every exclusion planted beside the
//! machines that must be listed.
use super::*;
use serde_json::{Value, json};

const SUFFIX: &str = "tail1f21e.ts.net";
const USER: u64 = 7;

fn node(id: &str, machine: &str, user: u64, ips: &[&str]) -> Value {
    json!({"ID": id, "DNSName": format!("{machine}.{SUFFIX}."), "OS": "macOS",
           "UserID": user, "TailscaleIPs": ips, "InNetworkMap": true, "Online": true})
}
fn status(peers: Vec<Value>) -> Value {
    let mut map = serde_json::Map::new();
    for (i, peer) in peers.into_iter().enumerate() {
        map.insert(format!("nodekey:{i:064}"), peer);
    }
    json!({"Version": "fixture", "BackendState": "Running",
           "CurrentTailnet": {"Name": "fixture", "MagicDNSSuffix": SUFFIX},
           "Self": node("nHOST", "mac-mini-old", USER, &["100.70.0.1", "fd7a:115c:a1e0::1"]),
           "Peer": map})
}
fn scope() -> Scope<'static> {
    Scope {
        host: "nHOST",
        viewer: "nVIEWER",
        user: USER,
    }
}
fn select_json(value: &Value, scope: Scope<'_>) -> Result<Directory, Error> {
    select(&Status::parse(&serde_json::to_vec(value).unwrap())?, scope)
}
fn names(directory: &Directory) -> Vec<&str> {
    directory.nodes.iter().map(|n| n.name.as_str()).collect()
}

#[test]
fn only_the_host_and_the_viewers_own_untagged_machines_are_listed() {
    let mut offline = node("nLINUX", "trj", USER, &["100.70.0.3"]);
    offline["Online"] = json!(false);
    offline["OS"] = json!("linux");
    let mut odd_os = node("nODD", "odd", USER, &["fd7a:115c:a1e0::9"]);
    odd_os["OS"] = json!("mac\nOS");
    let mut upper = node("nUPPER", "Mixed-Case", USER, &["100.70.0.8"]);
    upper["DNSName"] = json!(format!("Mixed-Case.{}.", SUFFIX.to_uppercase()));

    let mut planted = vec![
        node("nVIEWER", "iphone", USER, &["100.70.0.9"]), // the viewer itself
        node("nOTHER", "someone-else", 8, &["100.70.0.10"]), // another user
        node("nORPHAN", "tagless", 0, &["100.70.0.11"]),  // no user
    ];
    let mut tagged = node("nTAGGED", "server", USER, &["100.70.0.12"]);
    tagged["Tags"] = json!(["tag:server"]);
    let mut shared_in = node("nSHARED", "shared", USER, &["100.70.0.13"]);
    shared_in["AltSharerUserID"] = json!(9);
    let mut sharee = node("nSHAREE", "sharee", USER, &["100.70.0.14"]);
    sharee["ShareeNode"] = json!(true);
    let mut expired = node("nEXPIRED", "expired", USER, &["100.70.0.15"]);
    expired["Expired"] = json!(true);
    let mut unmapped = node("nUNMAPPED", "unmapped", USER, &["100.70.0.16"]);
    unmapped["InNetworkMap"] = json!(false);
    let mut foreign = node("nFOREIGN", "foreign", USER, &["100.70.0.17"]);
    foreign["DNSName"] = json!("foreign.other-tailnet.ts.net.");
    let mut nested = node("nNESTED", "a.b", USER, &["100.70.0.18"]);
    nested["DNSName"] = json!(format!("a.b.{SUFFIX}."));
    let off_tailnet = node("nLAN", "lan-only", USER, &["192.168.1.20", "fe80::1"]);
    let mut nameless = node("nNONAME", "x", USER, &["100.70.0.19"]);
    nameless["DNSName"] = json!("");
    let mut bad_id = node("n/../x", "bad-id", USER, &["100.70.0.20"]);
    bad_id["ID"] = json!("n/../x");
    planted.extend([
        tagged,
        shared_in,
        sharee,
        expired,
        unmapped,
        foreign,
        nested,
        off_tailnet,
        nameless,
        bad_id,
    ]);
    let mut peers = vec![
        node(
            "nSTUDIO",
            "mac-studio",
            USER,
            &["100.89.198.71", "fd7a:115c:a1e0::5a38:c648"],
        ),
        offline,
        odd_os,
        upper,
    ];
    peers.extend(planted);

    let directory = select_json(&status(peers), scope()).unwrap();
    assert_eq!(
        names(&directory),
        [
            "mac-mini-old.tail1f21e.ts.net",
            "mac-studio.tail1f21e.ts.net",
            "mixed-case.tail1f21e.ts.net",
            "odd.tail1f21e.ts.net",
            "trj.tail1f21e.ts.net",
        ]
    );
    assert!(!directory.truncated);
    let host = &directory.nodes[0];
    assert!(host.serving && host.online);
    assert_eq!(host.node_id, "nHOST");
    let studio = &directory.nodes[1];
    assert!(!studio.serving && studio.online);
    assert_eq!(studio.ipv4, Some("100.89.198.71".parse().unwrap()));
    assert_eq!(
        studio.ipv6,
        Some("fd7a:115c:a1e0::5a38:c648".parse().unwrap())
    );
    assert_eq!(studio.os, "macOS");
    assert_eq!(
        directory.nodes[3].os, "",
        "an unusual OS label is not passed on"
    );
    assert_eq!(directory.nodes[3].ipv4, None);
    let trj = &directory.nodes[4];
    assert!(!trj.online);
    assert_eq!(trj.os, "linux");
    assert_eq!(trj.ipv6, None);
}

#[test]
fn a_tagged_viewer_sees_only_the_host() {
    let peers = vec![node("nSTUDIO", "mac-studio", USER, &["100.70.0.2"])];
    let directory = select_json(&status(peers), Scope { user: 0, ..scope() }).unwrap();
    assert_eq!(names(&directory), ["mac-mini-old.tail1f21e.ts.net"]);
}

#[test]
fn the_daemon_must_still_be_the_admitted_host_and_running() {
    let base = status(vec![node("nSTUDIO", "mac-studio", USER, &["100.70.0.2"])]);
    let other_host = Scope {
        host: "nSOMEONE",
        ..scope()
    };
    assert_eq!(select_json(&base, other_host), Err(Error::IdentityChanged));
    let mut stopped = base.clone();
    stopped["BackendState"] = json!("Stopped");
    assert_eq!(
        select_json(&stopped, scope()),
        Err(Error::BackendNotRunning)
    );
    let mut no_suffix = base.clone();
    no_suffix["CurrentTailnet"] = json!(null);
    assert_eq!(
        select_json(&no_suffix, scope()),
        Err(Error::MalformedMetadata)
    );
    let mut unnamed_host = base.clone();
    unnamed_host["Self"]["DNSName"] = json!("");
    assert_eq!(
        select_json(&unnamed_host, scope()),
        Err(Error::MalformedMetadata)
    );
    assert_eq!(
        Status::parse(b"{\"BackendState\":\"Running\"").err(),
        Some(Error::MalformedMetadata)
    );
    let mut huge = vec![b' '; crate::metadata::STATUS_BYTES + 1];
    huge[0] = b'{';
    assert_eq!(Status::parse(&huge).err(), Some(Error::MalformedMetadata));
}

#[test]
fn duplicates_are_dropped_and_large_tailnets_are_truncated_visibly() {
    let mut peers = vec![
        node("nA", "same", USER, &["100.70.0.2"]),
        node("nB", "same", USER, &["100.70.0.3"]),
        node("nHOST", "host-twin", USER, &["100.70.0.4"]),
        node("nC", "unique", USER, &["100.70.0.5"]),
    ];
    let directory = select_json(&status(peers.clone()), scope()).unwrap();
    assert_eq!(
        names(&directory),
        ["mac-mini-old.tail1f21e.ts.net", "unique.tail1f21e.ts.net"]
    );

    peers = (0..300)
        .map(|i| {
            node(
                &format!("n{i:03}"),
                &format!("m{i:03}"),
                USER,
                &["100.70.1.1"],
            )
        })
        .collect();
    let directory = select_json(&status(peers), scope()).unwrap();
    assert_eq!(directory.nodes.len(), MAX_NODES);
    assert!(directory.truncated);
    assert!(directory.nodes[0].serving);
    assert_eq!(directory.nodes[1].name, "m000.tail1f21e.ts.net");
    assert_eq!(directory.nodes[255].name, "m254.tail1f21e.ts.net");
}

#[test]
fn debug_output_names_no_machine() {
    let directory = select_json(
        &status(vec![node("nSTUDIO", "mac-studio", USER, &["100.70.0.2"])]),
        scope(),
    )
    .unwrap();
    let text = format!("{directory:?} {:?}", directory.nodes);
    for secret in ["mac-studio", "mac-mini-old", "nSTUDIO", "100.70"] {
        assert!(!text.contains(secret), "{text}");
    }
}
