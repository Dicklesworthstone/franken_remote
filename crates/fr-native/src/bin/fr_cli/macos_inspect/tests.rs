//! Portable command-boundary tests; no macOS runtime or identity is simulated.
use super::*;
use crate::options::{InspectOptions, InspectProbe, SavedSelection};
use std::path::PathBuf;

fn options(node: &str, by_name: bool, port: u16) -> Options {
    Options {
        command: Command::Inspect(InspectOptions {
            saved: None,
            node: node.into(),
            by_name,
            port,
            probe: None,
        }),
        json: true,
        socket: None,
    }
}

#[test]
fn selectors_preserve_exact_identity_and_configured_port() {
    let stable = options("n-2", false, 8443);
    let request = prepare(&stable).unwrap();
    assert!(matches!(request.selector, PeerSelector::StableId("n-2")));
    assert_eq!(request.port, 8443);
    let named = options("remote.fixture.ts.net", true, 9443);
    let request = prepare(&named).unwrap();
    assert!(matches!(
        request.selector,
        PeerSelector::Name("remote.fixture.ts.net")
    ));
    assert_eq!(request.port, 9443);
}

#[test]
fn a_configured_socket_is_not_a_preflight_refusal() {
    // The open-source tailscaled's socket: read and credential-checked at run
    // time by fr-tailnet, never refused or ignored here.
    let mut custom = options("n-2", false, 8443);
    custom.socket = Some(PathBuf::from("/var/run/tailscaled.socket"));
    let request = prepare(&custom).unwrap();
    assert!(matches!(request.selector, PeerSelector::StableId("n-2")));
    assert_eq!(request.port, 8443);
}

#[test]
fn unsupported_modes_refuse_in_the_pure_preflight() {
    let mut saved = options("workstation", false, 8443);
    let Command::Inspect(inspect) = &mut saved.command else {
        unreachable!()
    };
    inspect.saved = Some(SavedSelection { file: None });
    assert_eq!(prepare(&saved).unwrap_err().code, "saved_hosts_unsupported");

    let mut probed = options("n-2", false, 8443);
    let Command::Inspect(inspect) = &mut probed.command else {
        unreachable!()
    };
    inspect.probe = Some(InspectProbe {
        roots: PathBuf::from("/tmp/roots.pem"),
        ipv6: true,
    });
    let failure = prepare(&probed).unwrap_err();
    assert_eq!(failure.code, "platform_unavailable");
    assert_eq!(failure.exit, 2);
    assert!(!failure.unknown_effect);
}

#[test]
fn unrelated_commands_cannot_become_an_identity_inspection() {
    let options = Options {
        command: Command::Hosts,
        json: true,
        socket: None,
    };
    assert_eq!(prepare(&options).unwrap_err().code, "invalid_arguments");
}

fn data(path: PeerTransportPath) -> RobotInspectData {
    identity(
        "n-2",
        "remote.fixture.ts.net",
        &[
            "100.64.0.2".parse().unwrap(),
            "fd7a:115c:a1e0::2".parse().unwrap(),
        ],
        9443,
        path,
    )
}

#[test]
fn output_names_only_checked_identity_and_keeps_host_capabilities_unknown() {
    let report = data(PeerTransportPath::Unknown);
    let json: serde_json::Value =
        serde_json::from_str(&render(report.clone(), 7, true).unwrap()).unwrap();
    assert_eq!(json["schema_version"], 1);
    assert_eq!(json["timestamp_unix_ms"], 7);
    assert_eq!(json["outcome"], "success");
    assert_eq!(json["data"]["state"], "identity_checked");
    assert_eq!(json["data"]["host"], "n-2");
    assert_eq!(json["data"]["port"], 9443);
    assert_eq!(json["data"]["addresses"].as_array().unwrap().len(), 2);
    assert!(json["data"]["displays_count"].is_null());
    assert!(json["data"]["requires_approval"].is_null());
    assert_eq!(json["data"]["transport_path"], "unknown");
    assert!(json["data"].get("capabilities").is_none());
    assert!(json["data"].get("tls_verified").is_none());
    let human = render(report, 7, false).unwrap();
    assert!(human.contains("State: identity_checked"));
    assert!(human.contains("Displays: unknown"));
    assert!(human.contains("Requires Approval: unknown"));
}

#[test]
fn path_projection_preserves_direct_relayed_and_unknown_samples() {
    for (path, expected, relayed) in [
        (
            PeerTransportPath::Direct {
                cur_addr: "192.0.2.1:41641".into(),
            },
            "direct (192.0.2.1:41641)",
            false,
        ),
        (
            PeerTransportPath::DerpRelayed {
                relay: "fixture-relay".into(),
            },
            "derp_relayed (fixture-relay)",
            true,
        ),
        (PeerTransportPath::Unknown, "unknown", false),
    ] {
        let report = data(path);
        assert_eq!(report.transport_path, expected);
        assert_eq!(report.is_derp_relayed, relayed);
        assert_eq!(report.state, "identity_checked");
    }
}

#[test]
fn terminal_controls_are_escaped_without_corrupting_json_identity() {
    let mut report = data(PeerTransportPath::DerpRelayed {
        relay: "region\u{1b}[2J".into(),
    });
    report.host = "node\u{1b}[2J".into();
    let json: serde_json::Value =
        serde_json::from_str(&render(report.clone(), 7, true).unwrap()).unwrap();
    assert_eq!(json["data"]["host"], "node\u{1b}[2J");
    let human = render(report, 7, false).unwrap();
    assert!(!human.contains('\u{1b}'));
    assert!(human.contains("node\\u{1b}[2J"));
    assert!(human.contains("region\\u{1b}[2J"));
}
