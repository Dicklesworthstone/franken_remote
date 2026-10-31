use super::{CapabilityInspectionData, Discovery, Failure, MAX_BATCH_CONCURRENCY, ProbeOutcome};
use serde::Serialize;
use std::fmt::Write as _;

#[derive(Serialize)]
pub(super) struct Report {
    scope: &'static str,
    discovered: usize,
    selected: usize,
    omitted: usize,
    excluded: Excluded,
    concurrency: usize,
    per_probe_budget_ms: u64,
    sweep_budget_ms: u64,
    budget_exhausted: bool,
    complete: bool,
    /// Metadata only: even a successful probe must be admitted afresh on connect.
    revalidate_on_connect: bool,
    hosts: Vec<Host>,
}
#[derive(Serialize)]
struct Excluded {
    shared: usize,
    expired: usize,
    unusable: usize,
}
#[derive(Serialize)]
struct Host {
    stable_id: String,
    certificate_name: String,
    state: &'static str,
    inspection: Option<CapabilityInspectionData>,
    error: Option<ProbeError>,
}
#[derive(Serialize)]
struct ProbeError {
    code: &'static str,
    next_action: &'static str,
}

impl Report {
    pub(super) fn new(
        snapshot: &Discovery,
        outcomes: Vec<ProbeOutcome<Result<CapabilityInspectionData, Failure>>>,
        expired: bool,
    ) -> Self {
        let selected = outcomes.len();
        let complete = outcomes
            .iter()
            .all(|r| matches!(r, ProbeOutcome::Completed(_)));
        let hosts = snapshot.peers().iter().zip(outcomes).map(|(peer, outcome)| {
            let (state, inspection, error) = match outcome {
                ProbeOutcome::Completed(Ok(data)) => ("capabilities_observed", Some(data), None),
                ProbeOutcome::Completed(Err(error)) => ("probe_failed", None, Some(ProbeError { code: error.code, next_action: error.next })),
                ProbeOutcome::Interrupted => ("probe_interrupted", None, Some(ProbeError {
                    code: "capability_discovery_deadline",
                    next_action: "The original discovery budget ended; explicitly inspect this peer for a new probe.",
                })),
                ProbeOutcome::NotStarted => ("not_probed", None, Some(ProbeError {
                    code: "capability_discovery_not_started",
                    next_action: "This peer was not contacted before the discovery budget ended; use fr inspect NODE_ID --probe --experimental-native.",
                })),
            };
            Host { stable_id: peer.stable_id().into(), certificate_name: peer.certificate_name().into(), state, inspection, error }
        }).collect();
        let excluded = snapshot.excluded();
        Self {
            scope: "tailnet_capability_sweep",
            discovered: snapshot.peers().len(),
            selected,
            omitted: snapshot.peers().len().saturating_sub(selected),
            excluded: Excluded {
                shared: excluded.shared,
                expired: excluded.expired,
                unusable: excluded.unusable,
            },
            concurrency: MAX_BATCH_CONCURRENCY,
            per_probe_budget_ms: 5_000,
            sweep_budget_ms: 20_000,
            budget_exhausted: expired,
            complete,
            revalidate_on_connect: true,
            hosts,
        }
    }
    pub(super) fn render_human(&self) -> String {
        let mut text = format!(
            "Capability discovery: {} selected of {} tailnet candidates; {} omitted by the limit.\n",
            self.selected, self.discovered, self.omitted
        );
        for host in &self.hosts {
            let _ = writeln!(
                text,
                "{} ({}) — {}",
                host.stable_id, host.certificate_name, host.state
            );
            if let Some(data) = &host.inspection {
                text.push_str(&data.render_human());
            }
            if let Some(error) = &host.error {
                let _ = writeln!(text, "  {}: {}", error.code, error.next_action);
            }
        }
        if self.budget_exhausted {
            text.push_str("The original sweep budget ended; unfinished probes were cancelled.\n");
        }
        text.push_str("Capability advertisements are snapshots; display inventory, approval policy and live codec admission remain unknown. Connecting revalidates identity and authority.\n");
        text
    }
}
pub(super) fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_client::robot::RobotInspectData;
    use fr_tailnet::{DiscoveredPeer, DiscoveryExclusions, PeerTransportPath};

    fn snapshot() -> Discovery {
        Discovery::from_parts(
            (0..5)
                .map(|index| {
                    DiscoveredPeer::from_parts(
                        format!("peer-{index}"),
                        format!("peer-{index}.tailnet.ts.net"),
                        vec!["100.64.0.2".parse().unwrap()],
                        PeerTransportPath::Unknown,
                    )
                })
                .collect(),
            DiscoveryExclusions {
                shared: 1,
                expired: 2,
                unusable: 3,
            },
            10,
            20,
        )
    }
    fn advertisement() -> CapabilityInspectionData {
        CapabilityInspectionData::after_acknowledged_probe(
            RobotInspectData {
                host: "peer-0".into(),
                certificate_name: "peer-0.tailnet.ts.net".into(),
                addresses: vec!["100.64.0.2".into()],
                port: 8443,
                is_derp_relayed: false,
                state: "discovered".into(),
                displays_count: None,
                requires_approval: None,
                transport_path: "unknown".into(),
            },
            fr_client::native::inspection_offer().unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn mixed_results_keep_identity_order_and_never_invent_readiness_or_offline() {
        let report = Report::new(
            &snapshot(),
            vec![
                ProbeOutcome::Completed(Ok(advertisement())),
                ProbeOutcome::Completed(Err(Failure::new(
                    "tls_certificate_rejected",
                    "Verify local roots.",
                    1,
                ))),
                ProbeOutcome::Interrupted,
                ProbeOutcome::NotStarted,
            ],
            true,
        );
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["selected"], 4);
        assert_eq!(value["discovered"], 5);
        assert_eq!(value["omitted"], 1);
        assert_eq!(value["excluded"]["shared"], 1);
        assert_eq!(value["complete"], false);
        assert_eq!(value["budget_exhausted"], true);
        assert_eq!(value["revalidate_on_connect"], true);
        for index in 0..4 {
            assert_eq!(value["hosts"][index]["stable_id"], format!("peer-{index}"));
        }
        let observed = &value["hosts"][0]["inspection"];
        assert_eq!(observed["live_capability_admission_verified"], false);
        assert_eq!(observed["observation_opened"], false);
        assert!(observed["displays_count"].is_null());
        assert!(observed["requires_approval"].is_null());
        assert_eq!(
            value["hosts"][1]["error"]["code"],
            "tls_certificate_rejected"
        );
        assert_eq!(value["hosts"][2]["state"], "probe_interrupted");
        assert_eq!(value["hosts"][3]["state"], "not_probed");
        for index in 1..4 {
            assert!(value["hosts"][index]["inspection"].is_null());
        }
        let human = report.render_human();
        assert!(human.contains("4 selected of 5"));
        assert!(!human.contains("offline"));
        assert!(!human.contains("desktop ready"));
        let envelope = fr_client::robot::RobotEnvelope::success(
            42,
            fr_client::robot::AcknowledgementStage::Observed,
            &report,
        );
        let wrapped: serde_json::Value =
            serde_json::from_str(&envelope.render_json().unwrap()).unwrap();
        assert_eq!(wrapped["schema_version"], 1);
        assert_eq!(wrapped["stage"], "observed");
        assert_eq!(wrapped["data"], value);
        assert!(wrapped.get("hosts").is_none());
    }
    #[test]
    fn a_completed_sweep_can_contain_only_typed_failures_without_claiming_host_success() {
        let report = Report::new(
            &snapshot(),
            vec![ProbeOutcome::Completed(Err(Failure::new(
                "host_not_answering",
                "Inspect host availability.",
                1,
            )))],
            false,
        );
        let value = serde_json::to_value(report).unwrap();
        assert_eq!(value["complete"], true);
        assert_eq!(value["hosts"][0]["state"], "probe_failed");
        assert!(value["hosts"][0]["inspection"].is_null());
    }
}
