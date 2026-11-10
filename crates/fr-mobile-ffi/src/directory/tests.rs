//! The directory result as the app reads it: typed outcomes, pins built from
//! exactly the listed addresses, and nothing invented. The fetch against a
//! real host is proven live (docs/ios-client.md); the C entry point's
//! refusals and a real failed fetch are in ffi/tests.rs.
use super::*;
use fr_tailnet::Error as T;
use fr_wire::refusal::Refused;
use std::net::{Ipv4Addr, Ipv6Addr};

fn entry(name: &str, v4: Option<u8>, v6: Option<u16>, serving: bool) -> Entry {
    Entry {
        name: name.into(),
        node_id: format!("n{}CNTRL", name.len()),
        os: "macOS".into(),
        online: true,
        serving,
        ipv4: v4.map(|i| Ipv4Addr::new(100, 70, 0, i)),
        ipv6: v6.map(|i| Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, i)),
    }
}
fn parsed(outcome: &Outcome) -> Value {
    serde_json::from_str(&to_json("mac-mini-old.tail1f21e.ts.net", outcome)).unwrap()
}

#[test]
fn a_listing_becomes_machines_with_pins_of_exactly_the_listed_addresses() {
    let listing = Listing {
        entries: vec![
            entry("mac-mini-old.tail1f21e.ts.net", Some(1), Some(1), true),
            entry("mac-studio.tail1f21e.ts.net", None, Some(2), false),
            entry("trj.tail1f21e.ts.net", Some(3), None, false),
            // A valid DNS name the app cannot dial by name: listed, no pin.
            entry("odd.sub.tail1f21e.ts.net", Some(4), None, false),
        ],
        truncated: true,
    };
    let value = parsed(&Outcome::Listed(listing));
    assert_eq!(value["status"], "ok");
    assert_eq!(value["via"], "mac-mini-old.tail1f21e.ts.net");
    assert_eq!(value["truncated"], true);
    let machines = value["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 4, "every listed machine, none invented");
    assert_eq!(machines[0]["serving"], true);
    assert_eq!(
        machines[0]["pin"],
        "fr-pin-v1 mac-mini-old.tail1f21e.ts.net 100.70.0.1 fd7a:115c:a1e0::1"
    );
    assert_eq!(machines[1]["name"], "mac-studio.tail1f21e.ts.net");
    assert_eq!(machines[1]["ipv4"], Value::Null);
    assert_eq!(machines[1]["ipv6"], "fd7a:115c:a1e0::2");
    assert_eq!(
        machines[1]["pin"],
        "fr-pin-v1 mac-studio.tail1f21e.ts.net fd7a:115c:a1e0::2"
    );
    assert_eq!(machines[2]["ipv6"], Value::Null);
    assert_eq!(
        machines[2]["pin"],
        "fr-pin-v1 trj.tail1f21e.ts.net 100.70.0.3"
    );
    assert_eq!(machines[3]["pin"], Value::Null);
    assert_eq!(machines[3]["os"], "macOS");
    assert_eq!(machines[3]["online"], true);
    // Each pin round-trips as the session parses it, for exactly its name.
    for machine in &machines[..3] {
        let pin: HostPin = machine["pin"].as_str().unwrap().parse().unwrap();
        assert_eq!(pin.name().as_str(), machine["name"].as_str().unwrap());
    }
}

#[test]
fn refusals_and_failures_stay_typed() {
    let ok = Listing {
        entries: vec![entry("m.tail1f21e.ts.net", Some(1), None, true)],
        truncated: false,
    };
    let refused = |reason| {
        Ok(Err(StartupError::ClientStartup(
            fr_client::startup::Error::Protocol(negotiation::Error::Refused(Refused::connection(
                reason,
            ))),
        )))
    };
    assert_eq!(outcome(Ok(Ok(ok.clone()))), Outcome::Listed(ok));
    for reason in [
        Reason::DirectoryDisabled,
        Reason::DirectoryUnavailable,
        Reason::PermissionDenied,
        Reason::RequiredCapability,
    ] {
        assert_eq!(outcome(refused(reason)), Outcome::Refused(reason));
    }
    // A host that answered without the marker (an older frd).
    assert_eq!(
        outcome(Ok(Err(StartupError::ClientStartup(
            fr_client::startup::Error::Protocol(negotiation::Error::RequiredCapability)
        )))),
        Outcome::Refused(Reason::RequiredCapability)
    );
    assert_eq!(
        outcome(Err(ConnectionError::Tailnet(T::NameUnresolved))),
        Outcome::Failed(end::NAME_UNRESOLVED, "Tailnet(NameUnresolved)".into())
    );
    assert_eq!(
        outcome(Err(ConnectionError::Tailnet(T::PeerCertificateRejected))).clone_code(),
        end::TLS_REFUSED
    );
    assert_eq!(
        outcome(Err(ConnectionError::Cancelled)).clone_code(),
        end::CANCELLED
    );
    assert_eq!(
        outcome(Ok(Err(StartupError::Expired))).clone_code(),
        end::SESSION_FAILED
    );
    assert_eq!(
        outcome(Err(ConnectionError::InvalidConfiguration)).clone_code(),
        end::INTERNAL
    );
    // Planted negative: a listing is never reported for a refusal.
    assert!(!matches!(
        outcome(refused(Reason::DirectoryDisabled)),
        Outcome::Listed(_)
    ));

    let value = parsed(&Outcome::Refused(Reason::DirectoryDisabled));
    assert_eq!(value["status"], "refused");
    assert_eq!(value["reason"], "directory_disabled");
    assert!(value.get("machines").is_none());
    let value = parsed(&Outcome::Failed(
        end::NAME_UNRESOLVED,
        "Tailnet(NameUnresolved)".into(),
    ));
    assert_eq!(value["status"], "failed");
    assert_eq!(value["end"], end::NAME_UNRESOLVED);
    assert_eq!(value["detail"], "Tailnet(NameUnresolved)");
}

#[test]
fn every_refusal_has_its_own_name() {
    let reasons = [
        Reason::InvalidMessage,
        Reason::UnsupportedVersion,
        Reason::UnsupportedProfile,
        Reason::RequiredCapability,
        Reason::InvalidLimits,
        Reason::InvalidSelection,
        Reason::PermissionDenied,
        Reason::LocalApprovalDenied,
        Reason::ApprovalExpired,
        Reason::ControlUnavailable,
        Reason::ResourceLimit,
        Reason::InvalidState,
        Reason::Expired,
        Reason::HostUnavailable,
        Reason::TailnetMembershipUnverifiable,
        Reason::DirectoryDisabled,
        Reason::DirectoryUnavailable,
    ];
    let mut names: Vec<_> = reasons.iter().map(|r| reason_name(*r)).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), reasons.len());
    assert_eq!(
        reason_name(Reason::DirectoryUnavailable),
        "directory_unavailable"
    );
}

impl Outcome {
    fn clone_code(&self) -> u32 {
        match self {
            Self::Failed(code, _) => *code,
            other => panic!("not a failure: {other:?}"),
        }
    }
}
