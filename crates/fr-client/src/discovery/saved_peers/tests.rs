use super::*;
fn peer(alias: &str, id: &str, name: &str, port: u16) -> SavedPeer {
    SavedPeer::new(alias.into(), id.into(), name.into(), port).unwrap()
}
#[test]
fn sorted_roundtrip_preserves_only_identity_and_port() {
    let mut peers = SavedPeers::default();
    peers
        .add(peer("work", "n-work", "work.example.ts.net", 9443))
        .unwrap();
    peers
        .add(peer("home", "n-home", "home.example.ts.net", 8443))
        .unwrap();
    let bytes = peers.to_json().unwrap();
    assert_eq!(SavedPeers::from_json(&bytes).unwrap(), peers);
    assert_eq!(peers.list()[0].alias(), "home");
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["hosts"][0].as_object().unwrap().len(), 4);
    assert_eq!(peers.get("work").unwrap().port(), 9443);
    peers.remove("work").unwrap();
    assert!(peers.get("work").is_none());
    assert_eq!(peers.remove("work"), Err(SavedPeerError::NotFound));
}
#[test]
fn record_validation_refuses_ambiguous_or_unbounded_fields() {
    for alias in [
        "",
        "UPPER",
        "-option",
        "with space",
        "path/name",
        "a@b",
        "a\n",
    ] {
        assert!(SavedPeer::new(alias.into(), "n-peer".into(), "host.ts.net".into(), 8443).is_err());
    }
    for name in [
        "",
        "localhost",
        "HOST.ts.net",
        "host.ts.net.",
        "x..ts.net",
        "-x.ts.net",
        "x-.ts.net",
        "fr://host.ts.net",
        "100.64.0.2:8443",
        "x.ts.net?token=a",
    ] {
        assert!(SavedPeer::new("home".into(), "n-peer".into(), name.into(), 8443).is_err());
    }
    for id in ["", "-x", "a b", "x/y", "secret@host", "x\n"] {
        assert!(SavedPeer::new("home".into(), id.into(), "host.ts.net".into(), 8443).is_err());
    }
    assert!(SavedPeer::new("a".repeat(65), "n-peer".into(), "host.ts.net".into(), 8443).is_err());
    assert!(SavedPeer::new("home".into(), "n".repeat(129), "host.ts.net".into(), 8443).is_err());
    assert!(SavedPeer::new("home".into(), "n-peer".into(), "host.ts.net".into(), 0).is_err());
}
#[test]
fn persisted_records_cannot_bypass_duplicate_checks_or_schema() {
    let mut peers = SavedPeers::default();
    peers
        .add(peer("home", "n-peer", "host.ts.net", 8443))
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&peers.to_json().unwrap()).unwrap();
    for field in ["alias", "stable_id", "certificate_name"] {
        let mut other = serde_json::json!({"alias":"work", "stable_id":"n-other", "certificate_name":"other.ts.net", "port":8443});
        other[field] = value["hosts"][0][field].clone();
        let mut bad = value.clone();
        bad["hosts"].as_array_mut().unwrap().push(other);
        assert_eq!(
            SavedPeers::from_json(&serde_json::to_vec(&bad).unwrap()),
            Err(SavedPeerError::Duplicate)
        );
    }
    for (field, v) in [
        ("port", serde_json::json!(0)),
        ("alias", serde_json::json!("")),
        ("certificate_name", serde_json::json!("host.ts.net.")),
    ] {
        let mut bad = value.clone();
        bad["hosts"][0][field] = v;
        assert_eq!(
            SavedPeers::from_json(&serde_json::to_vec(&bad).unwrap()),
            Err(SavedPeerError::InvalidRecord)
        );
    }
    let mut bad = value.clone();
    bad["schema_version"] = 2.into();
    assert_eq!(
        SavedPeers::from_json(&serde_json::to_vec(&bad).unwrap()),
        Err(SavedPeerError::InvalidDocument)
    );
    let mut bad = value;
    bad["hosts"][0]["control"] = true.into();
    assert_eq!(
        SavedPeers::from_json(&serde_json::to_vec(&bad).unwrap()),
        Err(SavedPeerError::InvalidDocument)
    );
    assert_eq!(
        SavedPeers::from_json(br#"{"schema_version":1,"schema_version":1,"hosts":[]}"#),
        Err(SavedPeerError::InvalidDocument)
    );
}
#[test]
fn bounds_apply_before_parsing_and_after_population() {
    assert_eq!(
        SavedPeers::from_json(&vec![b' '; MAX_SAVED_PEER_BYTES + 1]),
        Err(SavedPeerError::TooLarge)
    );
    let mut peers = SavedPeers::default();
    for i in 0..MAX_SAVED_PEERS {
        peers
            .add(peer(
                &format!("alias-{i}"),
                &format!("n-{i}"),
                &format!("host-{i}.ts.net"),
                8443,
            ))
            .unwrap();
    }
    let mut json: serde_json::Value = serde_json::from_slice(&peers.to_json().unwrap()).unwrap();
    let duplicate = json["hosts"][0].clone();
    json["hosts"].as_array_mut().unwrap().push(duplicate);
    assert_eq!(
        SavedPeers::from_json(&serde_json::to_vec(&json).unwrap()),
        Err(SavedPeerError::Capacity)
    );
    assert_eq!(
        peers.add(peer("overflow", "n-extra", "extra.ts.net", 8443)),
        Err(SavedPeerError::Capacity)
    );
}
