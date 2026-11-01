//! A local machine choice before any remote connection or source selection.
//! The immutable displayed identity is re-resolved by the existing connector.
use super::{Cell, Connection, Cx, Failure, LocalApi, Runtime, Shutdown, failure, tailnet};
use crate::saved_hosts::Selection;
use fr_client::discovery::picker::{Candidate, Catalog, PathHint};
use fr_native::host_picker::{self, HostPicker};
use std::{
    thread,
    time::{Duration, Instant},
};

const POLL: Duration = Duration::from_millis(10);
const CLEANUP: Duration = Duration::from_secs(2);

#[allow(clippy::too_many_arguments)] // The existing original CLI runtime and signal owner.
pub(super) fn select(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: &LocalApi,
    connection: &Connection,
    json: bool,
) -> Result<Selection, Failure> {
    if !connection.target.node.is_empty()
        || connection.target.saved.is_some()
        || connection.target.by_name
        || connection.target.port == 0
    {
        return Err(Failure::new(
            "invalid_host_choice",
            "Use --choose-host in place of the positional host, without --saved or --by-name.",
            2,
        ));
    }
    let display = super::x_display(connection)?;
    let snapshot = runtime
        .block_on(shutdown.run(cx, stopped, api.discover(cx)))
        .map_err(tailnet)?;
    if stopped.get() || cx.is_cancel_requested() {
        return Err(cancelled());
    }
    let catalog = catalog(&snapshot)?;
    if catalog.is_empty() {
        return Err(failure(
            "host_choice_empty",
            "The installed Tailscale daemon returned no selectable peers. Check its account, peer visibility and key expiry; no remote connection was opened.",
        ));
    }
    let rows = rows(&catalog)?;
    let mut picker = HostPicker::start(&display, &rows).map_err(|_| unavailable())?;
    let result = runtime.block_on(shutdown.run(cx, stopped, wait(cx, &mut picker)));
    // Cancellation fences the local choice immediately. The independent join
    // receipt, including failed-open paths, precedes any remote session owner.
    picker.cancel();
    if !finish(&picker) {
        return Err(failure(
            "host_picker_cleanup_incomplete",
            "The original native host picker has not finished cleanup. No remote connection was opened.",
        ));
    }
    if stopped.get() || cx.is_cancel_requested() {
        return Err(cancelled());
    }
    let candidate = catalog.get(result?).map_err(|_| invalid_snapshot())?;
    let selected = Selection::from_candidate(candidate, connection.target.port);
    print!("{}", selected_report(candidate, selected.port, json));
    Ok(selected)
}

fn catalog(snapshot: &fr_tailnet::Discovery) -> Result<Catalog, Failure> {
    // Discovery owns its canonical bounds. The model independently checks its
    // immutable projection, including identity ambiguity; no address is copied.
    if snapshot.peers().len() > fr_client::discovery::picker::MAX_CANDIDATES {
        return Err(invalid_snapshot());
    }
    let candidates = snapshot
        .peers()
        .iter()
        .map(|peer| {
            Candidate::new(
                peer.stable_id().to_owned(),
                peer.certificate_name().to_owned(),
                match peer.transport_path() {
                    fr_tailnet::PeerTransportPath::Direct { .. } => PathHint::Direct,
                    fr_tailnet::PeerTransportPath::DerpRelayed { .. } => PathHint::Relayed,
                    fr_tailnet::PeerTransportPath::Unknown => PathHint::Unknown,
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| invalid_snapshot())?;
    Catalog::new(candidates).map_err(|_| invalid_snapshot())
}

fn rows(catalog: &Catalog) -> Result<Vec<host_picker::Row>, Failure> {
    catalog
        .candidates()
        .iter()
        .map(|candidate| {
            host_picker::Row::new(
                candidate.certificate_name(),
                candidate.stable_id(),
                match candidate.path_hint() {
                    PathHint::Direct => host_picker::PathHint::Direct,
                    PathHint::Relayed => host_picker::PathHint::Relayed,
                    PathHint::Unknown => host_picker::PathHint::Unknown,
                },
            )
            .map_err(|_| invalid_snapshot())
        })
        .collect()
}

async fn wait(cx: &Cx, picker: &mut HostPicker) -> Result<usize, Failure> {
    let clock = cx.timer_driver().ok_or_else(unavailable)?;
    loop {
        if cx.is_cancel_requested() || picker.status() == host_picker::Status::Cancelled {
            picker.cancel();
            return Err(cancelled());
        }
        match picker.poll() {
            Ok(Some(index)) => return Ok(index),
            Ok(None) => {}
            Err(_) if picker.status() == host_picker::Status::Cancelled => {
                return Err(cancelled());
            }
            Err(_) => return Err(unavailable()),
        }
        // No X-server call or blocking join runs on this runtime turn.
        asupersync::time::sleep(clock.now(), POLL).await;
    }
}

fn finish(picker: &HostPicker) -> bool {
    let Some(until) = Instant::now().checked_add(CLEANUP) else {
        return false;
    };
    while !picker.is_complete() && Instant::now() < until {
        thread::sleep(POLL);
    }
    picker.is_complete()
}
fn cancelled() -> Failure {
    Failure::new(
        "host_choice_cancelled",
        "Host selection was cancelled. No remote connection or input request was opened.",
        130,
    )
}
fn unavailable() -> Failure {
    failure(
        "host_picker_unavailable",
        "Check the local X11 display, authorization, window focus and keyboard mapping, then make a new explicit selection. No remote connection was opened.",
    )
}
fn invalid_snapshot() -> Failure {
    failure(
        "host_choice_invalid_snapshot",
        "The discovery snapshot contains an invalid, ambiguous or out-of-bounds peer identity. Refresh installed Tailscale metadata before choosing again; no remote connection was opened.",
    )
}

fn selected_report(candidate: &Candidate, port: u16, json: bool) -> String {
    if json {
        format!(
            "{}\n",
            serde_json::json!({
                "schema_version": 1, "event": "host_selected",
                "node_id": candidate.stable_id(),
                "certificate_name": candidate.certificate_name(), "port": port,
                "selection_only": true, "fresh_lookup_required": true,
                "remote_session_started": false, "input_requested": false
            })
        )
    } else {
        format!(
            "Selected {} ({}), port {port}. Rechecking identity and host admission.\n",
            candidate.certificate_name(),
            candidate.stable_id()
        )
    }
}

#[cfg(test)]
#[path = "host_choice/tests.rs"]
mod tests;
