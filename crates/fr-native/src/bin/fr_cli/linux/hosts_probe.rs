//! Explicit, bounded live capability discovery of installed-daemon candidates.
//! Each probe has its own cancellation/LocalAPI owner; one peer closing cannot
//! cancel the sweep or borrow another peer's identity, lookup slot or deadline.
use super::{Cell, Cx, Failure, LocalApi, Runtime, Shutdown, failure, read_roots, robot, tailnet};
use crate::options::HostsProbeOptions;
use asupersync::{tls::Certificate, types::Budget};
use fr_client::{
    discovery::batch::{MAX_BATCH_CONCURRENCY, ProbeOutcome, probe_batch},
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
        let expired = Cell::new(false);
        let started = cx.now();
        let until = started
            .as_nanos()
            .checked_add(20_000_000_000)
            .ok_or_else(|| {
                failure(
                    "capability_discovery_clock",
                    "The original discovery clock cannot represent its bounded deadline.",
                )
            })?;
        let snapshot = api.discover(cx).await.map_err(tailnet)?;
        let peers = &snapshot.peers()[..snapshot.peers().len().min(options.limit)];
        let batch = probe_batch(
            peers.len(),
            |index| {
                probe_one(
                    runtime,
                    socket,
                    peers.get(index).expect("bounded original peer index"),
                    &roots,
                    options,
                )
            },
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
        Ok(Report::new(&snapshot, outcomes, expired.get()))
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

async fn probe_one(
    runtime: &Runtime,
    socket: Option<&Path>,
    peer: &DiscoveredPeer,
    roots: &[Certificate],
    options: &HostsProbeOptions,
) -> Result<CapabilityInspectionData, Failure> {
    // LocalApi clones intentionally share a one-lookup guard. These are four
    // independent, bounded owners of the SAME locally selected protected
    // endpoint, so concurrent peers neither race that slot nor bypass checks.
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
