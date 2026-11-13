//! Private trusted-reader fixtures: no external constructor or socket bypass.
use super::*;
use serde_json::{Value, json};

fn verified() -> VerifiedTarget {
    let mut status: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/linux-1.102.3/status.json"
    ))
    .unwrap();
    status["Self"]["DNSName"] = json!("local.fixture.ts.net.");
    let peer = status["Peer"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    peer["DNSName"] = json!("remote.fixture.ts.net.");
    peer.as_object_mut().unwrap().remove("KeyExpiry");
    VerifiedTarget {
        identity: metadata::outbound::resolve(
            &metadata::Status::parse(&serde_json::to_vec(&status).unwrap()).unwrap(),
            PeerSelector::StableId("n-2"),
        )
        .unwrap(),
        source_identity: 7, // Synthetic authenticated source ID inside this private test.
        issued_us: 100,
        wall_issued_us: 1_500_000,
        whois_expiry: None,
    }
}

#[test]
fn whois_expiry_caps_the_same_original_lifetime_as_both_status_keys() {
    let mut read = verified();
    read.identity.host.host.expiry = Some(metadata::Text("1970-01-01T00:00:04Z".into()));
    read.identity.peer.expiry = Some(metadata::Text("1970-01-01T00:00:03Z".into()));
    read.whois_expiry = Some("1970-01-01T00:00:02Z".into());
    let target = read.finish(Arc::new(())).unwrap();
    assert_eq!(target.expires_us(), 500_100);
    assert_eq!(target.key_expires_us, Some(2_000_000));
    assert_eq!(target.check_at(500_099, 1_999_999), Ok(()));
    assert_eq!(target.check_at(500_100, 1_999_999), Err(Error::Expired));
    assert_eq!(target.check_at(200, 2_000_000), Err(Error::KeyExpired));

    // The third source can only tighten the already bounded lifetime. A later
    // WhoIs expiry cannot extend either status key or the original three seconds.
    let mut read = verified();
    read.identity.peer.expiry = Some(metadata::Text("1970-01-01T00:00:02Z".into()));
    read.whois_expiry = Some("1970-01-01T00:00:05Z".into());
    assert_eq!(read.finish(Arc::new(())).unwrap().expires_us(), 500_100);
    let mut read = verified();
    read.whois_expiry = Some("1970-01-01T00:00:05Z".into());
    assert_eq!(read.finish(Arc::new(())).unwrap().expires_us(), 3_000_100);
}

#[test]
fn expired_or_malformed_whois_expiry_cannot_mint_a_destination() {
    for expiry in [
        "not-a-date",
        "2026-99-01T00:00:00Z",
        "2026-01-01T00:00:00+24:00",
    ] {
        let mut read = verified();
        read.whois_expiry = Some(expiry.into());
        assert_eq!(
            read.finish(Arc::new(())).unwrap_err(),
            Error::MalformedMetadata
        );
    }
    let mut read = verified();
    read.whois_expiry = Some("1970-01-01T00:00:01Z".into());
    assert_eq!(read.finish(Arc::new(())).unwrap_err(), Error::KeyExpired);
    let mut read = verified();
    read.issued_us = u64::MAX;
    assert_eq!(read.finish(Arc::new(())).unwrap_err(), Error::Clock);
}

#[test]
fn original_deadline_origin_and_source_identity_never_refresh_implicitly() {
    let origin = Arc::new(());
    let original = verified().finish(origin.clone()).unwrap();
    assert_eq!(original.check_at(99, 1_500_000), Err(Error::Clock));
    // A wall clock stepped back holds at issue: still valid, and still
    // expired at the original monotonic deadline.
    assert_eq!(original.check_at(100, 1_499_999), Ok(()));
    assert_eq!(original.check_at(3_000_100, 1_000_000), Err(Error::Expired));
    let mut far = verified();
    far.wall_issued_us = 61_500_001;
    let far = far.finish(origin.clone()).unwrap();
    assert_eq!(far.check_at(100, 1_500_000), Err(Error::Clock));
    assert_eq!(original.check_at(3_000_099, 1_500_000), Ok(()));
    assert_eq!(original.check_at(3_000_100, 1_500_000), Err(Error::Expired));

    let mut later = verified();
    later.issued_us = 1_000_100;
    later.wall_issued_us = 2_500_000;
    let next = later.finish(origin.clone()).unwrap();
    assert!(original.same_identity(&next));
    assert_eq!(original.check_at(3_000_100, 2_500_000), Err(Error::Expired));
    assert_eq!(next.check_at(3_000_100, 2_500_000), Ok(()));

    let mut replacement = verified();
    replacement.source_identity = 8;
    assert!(!original.same_identity(&replacement.finish(origin).unwrap()));
    assert!(!original.same_identity(&verified().finish(Arc::new(())).unwrap()));
}
