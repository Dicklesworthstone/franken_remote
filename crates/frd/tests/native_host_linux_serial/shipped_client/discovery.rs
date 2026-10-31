//! Shipped CLI capability discovery over real UDP/TLS and the production host
//! dispatcher. Installed-daemon metadata, CA and ingress remain fixtures.
use super::*;
use serde_json::{Value, json};

fn candidates() -> Vec<u8> {
    let mut status: Value = serde_json::from_slice(&client_status(false)).unwrap();
    for (id, node, digit, ip) in [
        ("n-a-silent", 3, "3", "100.64.0.3"),
        ("n-z-unreachable", 4, "4", "100.64.0.4"),
    ] {
        let key = format!("nodekey:{}", digit.repeat(64));
        status["Peer"][&key] = json!({
            "ID": id, "NodeID": node, "PublicKey": key, "UserID": 7,
            "TailscaleIPs": [ip], "InNetworkMap": true,
            "DNSName": format!("{id}.fixture.ts.net."),
        });
    }
    serde_json::to_vec(&status).unwrap()
}

fn run_probe(fr: &Path, api: &Path, roots: &Path) -> std::process::Output {
    wait_for(
        Command::new(fr)
            .args(["hosts", "--probe", "--experimental-native", "--socket"])
            .arg(api)
            .arg("--trust-roots")
            .arg(roots)
            .args(["--port", &address().port().to_string(), "--json"])
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
        Duration::from_secs(25),
    )
}

#[test]
#[ignore = "explicit isolated user/mount/network namespace; synthetic ingress; shipped fr binary"]
fn shipped_fr_hosts_probe_keeps_peer_failures_separate_and_never_opens_observation() {
    // Hosted verifies the namespace marker before any addresses are added.
    let host = recovery::Hosted::new();
    let added = Command::new("ip")
        .args(["addr", "add", "100.64.0.3/32", "dev", "lo"])
        .output()
        .unwrap();
    assert!(
        added.status.success(),
        "isolated fixture address: {}",
        String::from_utf8_lossy(&added.stderr)
    );
    // A real bound socket keeps the first candidate silent without generating
    // an ICMP refusal. The last candidate has no route in this namespace.
    let _silent = UdpSocket::bind(("100.64.0.3", address().port())).unwrap();
    let api = ClientApi::with_status(candidates());
    let fr = shipped_client();
    let output = run_probe(&fr, &api.path, &host.roots);
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["outcome"], "success");
    assert_eq!(report["stage"], "observed");
    let data = &report["data"];
    assert_eq!(data["scope"], "tailnet_capability_sweep");
    assert_eq!(data["discovered"], 3);
    assert_eq!(data["selected"], 3);
    assert_eq!(data["omitted"], 0);
    assert_eq!(data["complete"], true);
    assert_eq!(data["budget_exhausted"], false);
    assert_eq!(data["revalidate_on_connect"], true);
    let hosts = data["hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 3);
    assert_eq!(hosts[0]["stable_id"], "n-a-silent");
    assert_eq!(hosts[0]["state"], "probe_failed");
    assert_eq!(hosts[0]["error"]["code"], "host_not_answering");
    assert!(hosts[0]["inspection"].is_null());
    assert_eq!(hosts[2]["stable_id"], "n-z-unreachable");
    assert_eq!(hosts[2]["state"], "probe_failed");
    assert_eq!(hosts[2]["error"]["code"], "tailnet_route_unavailable");
    assert!(hosts[2]["inspection"].is_null());
    assert_advertisement(&hosts[1]);
    host.wait_count(1, |event| matches!(event, Event::PeerFinished { .. }));
    assert_eq!(
        host.count(|event| matches!(event, Event::ShareEnded { .. })),
        0
    );
    assert!(
        !host.trace.exists(),
        "the sweep launched the capture worker"
    );

    // A real later observation consumes the original cold source factory. This
    // also proves the absent trace above was a live launch sentinel, not a
    // broken fixture or a failed host quietly presented as a successful probe.
    let output = run_displays(&fr, &api.path, &host.roots);
    assert!(output.status.success(), "{:?}", output);
    let displays: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(displays["displays"].as_array().unwrap().len(), 1);
    assert!(
        host.trace.exists(),
        "later observation did not start the source"
    );
    host.finish();
}

fn assert_advertisement(host: &Value) {
    assert_eq!(host["stable_id"], "n-host");
    assert_eq!(host["state"], "capabilities_observed");
    assert!(host["error"].is_null());
    let inspection = &host["inspection"];
    assert_eq!(inspection["scope"], "advertised_capability_intersection");
    assert_eq!(inspection["completion"], "inspection_acknowledged");
    for field in [
        "observation_opened",
        "input_requested",
        "native_workers_started",
        "live_capability_admission_verified",
    ] {
        assert_eq!(inspection[field], false, "{field}: {inspection}");
    }
    assert!(inspection["displays_count"].is_null());
    assert!(inspection["requires_approval"].is_null());
    assert!(
        inspection["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cap| { cap["name"] == fr_wire::inspection::CAPABILITY && cap["version"] == 1 })
    );
}
