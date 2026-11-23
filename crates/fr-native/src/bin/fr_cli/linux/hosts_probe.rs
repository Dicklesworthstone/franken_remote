//! Explicit, bounded live capability discovery of installed-daemon candidates.
//! Each probe has its own cancellation/LocalAPI owner; one peer closing cannot
//! cancel the sweep or borrow another peer's identity, lookup slot or deadline.
use super::{Cell, Cx, Failure, LocalApi, Runtime, Shutdown, failure, read_roots, robot, tailnet};
use crate::options::HostsProbeOptions;
use asupersync::{
    tls::Certificate,
    types::{Budget, Time},
};
#[cfg(not(target_os = "macos"))]
use fr_client::discovery::batch::probe_batch as probe_candidates;
#[cfg(target_os = "macos")]
use fr_client::discovery::batch::probe_serial as probe_candidates;
use fr_client::{
    discovery::batch::{MAX_BATCH_CONCURRENCY, ProbeOutcome},
    robot::{AcknowledgementStage, RobotEnvelope, inspect::capabilities::CapabilityInspectionData},
};
use fr_tailnet::{DiscoveredPeer, Discovery};
use std::{
    future::{Future, poll_fn},
    path::Path,
    pin::pin,
    time::Duration,
};

#[path = "hosts_probe/report.rs"]
mod report;
use report::Report;

const SWEEP_BUDGET: Duration = Duration::from_secs(20);
// The signed Mac application's LocalAPI has one process-wide lookup credit.
// Preserve it rather than making concurrently started probes fail each other.
const PROBE_CONCURRENCY: usize = if cfg!(target_os = "macos") {
    1
} else {
    MAX_BATCH_CONCURRENCY
};
type Outcomes = Vec<ProbeOutcome<Result<CapabilityInspectionData, Failure>>>;

#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: &LocalApi,
    socket: Option<&Path>,
    options: &HostsProbeOptions,
    json: bool,
) -> Result<String, Failure> {
    // Validate local trust before any remote work. No peer can supply roots.
    let roots = read_roots(&options.transport.roots)?;
    let operation = async {
        let started = cx.now();
        let snapshot = api.discover(cx).await.map_err(tailnet)?;
        let (outcomes, expired) =
            sweep(runtime, cx, socket, &snapshot, &roots, options, started).await?;
        Ok(Report::new(&snapshot, outcomes, expired))
    };
    let result: Result<Report, Failure> = runtime.block_on(shutdown.run(cx, stopped, operation));
    if stopped.get() {
        return Err(Failure::new(
            "cancelled",
            "Capability discovery cancelled; no observation was opened.",
            130,
        ));
    }
    let report = result?;
    let human = (!json).then(|| report.render_human());
    let envelope = RobotEnvelope::success(
        report::now_unix_ms(),
        AcknowledgementStage::Observed,
        report,
    );
    if json {
        envelope.render_json().map_err(|_| {
            failure(
                "serialization_error",
                "Failed to render capability discovery.",
            )
        })
    } else {
        Ok(envelope.render_human(human.as_deref()))
    }
}

#[allow(clippy::too_many_arguments)]
async fn sweep(
    runtime: &Runtime,
    cx: &Cx,
    socket: Option<&Path>,
    snapshot: &Discovery,
    roots: &[Certificate],
    options: &HostsProbeOptions,
    started: Time,
) -> Result<(Outcomes, bool), Failure> {
    let expired = Cell::new(false);
    let until = started
        .as_nanos()
        .checked_add(20_000_000_000)
        .ok_or_else(|| {
            failure(
                "capability_discovery_clock",
                "The original discovery clock cannot represent its bounded deadline.",
            )
        })?;
    let peers = &snapshot.peers()[..snapshot.peers().len().min(options.limit)];
    let batch = probe_candidates(
        peers.len(),
        |index| probe_one(runtime, socket, &peers[index], roots, options),
        || {
            let now = cx.now().as_nanos();
            if now < started.as_nanos() || now >= until {
                expired.set(true);
            }
            !cx.is_cancel_requested() && !expired.get()
        },
    )
    .map_err(|_| {
        failure(
            "capability_discovery_limit",
            "Use a probe limit between 1 and 64.",
        )
    })?;
    let mut batch = pin!(batch);
    let mut timeout = pin!(asupersync::time::sleep(started, SWEEP_BUDGET));
    let outcomes = poll_fn(|task| {
        if timeout.as_mut().poll(task).is_ready() {
            expired.set(true);
        }
        batch.as_mut().poll(task)
    })
    .await;
    Ok((outcomes, expired.get()))
}

/// Explicit app discovery uses this same authenticated bounded sweep. The
/// immutable original snapshot supplies every identity; no row is filtered or
/// selected automatically from its result. App invocations use installed Tailscale.
#[cfg(any(
    feature = "linux-desktop",
    all(target_os = "macos", feature = "macos-desktop")
))]
pub(super) fn picker_catalog(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    snapshot: &Discovery,
    connection: &crate::options::Connection,
) -> Result<fr_client::discovery::picker::Catalog, Failure> {
    let options = HostsProbeOptions {
        port: connection.target.port,
        limit: fr_client::discovery::batch::MAX_BATCH_TARGETS,
        transport: crate::options::InspectProbe {
            roots: connection.target.roots.clone(),
            ipv6: connection.target.ipv6,
        },
    };
    let roots = read_roots(&options.transport.roots)?;
    let operation = sweep(runtime, cx, None, snapshot, &roots, &options, cx.now());
    let (outcomes, _) = runtime.block_on(shutdown.run(cx, stopped, operation))?;
    if stopped.get() || cx.is_cancel_requested() {
        return Err(crate::host_choice::cancelled());
    }
    let hints = outcomes.iter().map(probe_hint).collect::<Vec<_>>();
    crate::host_choice::catalog_with_probe_hints(snapshot, &hints)
}

#[cfg(any(
    feature = "linux-desktop",
    all(target_os = "macos", feature = "macos-desktop"),
    test
))]
fn probe_hint(
    outcome: &ProbeOutcome<Result<CapabilityInspectionData, Failure>>,
) -> fr_client::discovery::picker::ProbeHint {
    use fr_client::discovery::picker::ProbeHint;
    match outcome {
        ProbeOutcome::Completed(Ok(data)) => {
            let advertises = |offer: fr_wire::negotiation::Offer| {
                offer
                    .capabilities
                    .iter()
                    .filter(|cap| cap.required)
                    .all(|required| {
                        data.capabilities
                            .iter()
                            .any(|cap| cap.name == required.name && cap.version == required.version)
                    })
            };
            if advertises(fr_client::native::control_offer()) {
                ProbeHint::ControlAdvertised
            } else if advertises(fr_client::native::observation_offer()) {
                ProbeHint::ViewingAdvertised
            } else {
                ProbeHint::CapabilitiesObserved
            }
        }
        ProbeOutcome::Completed(Err(error)) => match error.code {
            "host_busy" => ProbeHint::HostBusy,
            "tls_identity_refused" | "tls_certificate_rejected" | "tailnet_identity_refused" => {
                ProbeHint::IdentityRefused
            }
            "host_not_answering" | "capability_inspection_timeout" => ProbeHint::NoAnswer,
            _ => ProbeHint::Failed,
        },
        ProbeOutcome::Interrupted => ProbeHint::Interrupted,
        ProbeOutcome::NotStarted => ProbeHint::NotProbed,
    }
}

async fn probe_one(
    runtime: &Runtime,
    socket: Option<&Path>,
    peer: &DiscoveredPeer,
    roots: &[Certificate],
    options: &HostsProbeOptions,
) -> Result<CapabilityInspectionData, Failure> {
    // Each probe retains the same locally selected protected endpoint. Linux
    // has at most four independent lookup owners; the Mac installed provider's
    // process-wide credit is preserved by the serial sweep above.
    let api = match socket {
        Some(path) => LocalApi::new(path).map_err(tailnet)?,
        None => LocalApi::installed(),
    };
    let cx = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| {
            failure(
                "runtime_unavailable",
                "Check local runtime resources before probing.",
            )
        })?;
    let identity = robot::inspection_identity(peer, peer.stable_id(), options.port);
    robot::probe::execute(
        cx,
        api,
        roots.to_vec(),
        peer,
        options.port,
        options.transport.ipv6,
        identity,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_client::{discovery::picker::ProbeHint, robot::RobotInspectData};

    #[test]
    fn picker_hints_require_the_whole_advertised_profile_and_never_guess_readiness() {
        let data = CapabilityInspectionData::after_acknowledged_probe(
            RobotInspectData {
                host: "n-original".into(),
                certificate_name: "original.fixture.ts.net".into(),
                addresses: vec!["100.64.0.2".into()],
                port: 9443,
                is_derp_relayed: false,
                state: "discovered".into(),
                displays_count: None,
                requires_approval: None,
                transport_path: "unknown".into(),
            },
            fr_client::native::inspection_offer().unwrap(),
        )
        .unwrap();
        assert!(
            !data.observation_opened
                && !data.input_requested
                && !data.live_capability_admission_verified
        );
        assert_eq!(
            probe_hint(&ProbeOutcome::Completed(Ok(data.clone()))),
            ProbeHint::ControlAdvertised
        );
        let mut viewing = data.clone();
        viewing
            .capabilities
            .retain(|cap| cap.name != fr_wire::attachment::INPUT_CAPABILITY);
        assert_eq!(
            probe_hint(&ProbeOutcome::Completed(Ok(viewing.clone()))),
            ProbeHint::ViewingAdvertised
        );
        viewing
            .capabilities
            .retain(|cap| cap.name != fr_wire::display::CAPABILITY);
        assert_eq!(
            probe_hint(&ProbeOutcome::Completed(Ok(viewing))),
            ProbeHint::CapabilitiesObserved
        );
        for (code, hint) in [
            ("host_busy", ProbeHint::HostBusy),
            ("tls_certificate_rejected", ProbeHint::IdentityRefused),
            ("host_not_answering", ProbeHint::NoAnswer),
            ("capability_inspection_timeout", ProbeHint::NoAnswer),
            ("unknown_failure", ProbeHint::Failed),
        ] {
            assert_eq!(
                probe_hint(&ProbeOutcome::Completed(Err(Failure::new(
                    code,
                    "No authority",
                    1
                )))),
                hint
            );
        }
        assert_eq!(
            probe_hint(&ProbeOutcome::Interrupted),
            ProbeHint::Interrupted
        );
        assert_eq!(probe_hint(&ProbeOutcome::NotStarted), ProbeHint::NotProbed);
    }
}
