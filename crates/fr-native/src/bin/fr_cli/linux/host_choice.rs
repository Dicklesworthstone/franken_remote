//! A local machine choice before any remote connection or source selection.
//! The immutable displayed identity is re-resolved by the existing connector.
use super::{Cell, Connection, Cx, Failure, LocalApi, Runtime, Shutdown, failure, tailnet};
use crate::{
    host_choice::{cancelled, catalog, invalid_snapshot, selected_report, validate},
    saved_hosts::Selection,
};
#[cfg(test)]
use fr_client::discovery::picker::Candidate;
use fr_client::discovery::picker::{Catalog, PathHint};
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
    validate(&connection.target)?;
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
fn unavailable() -> Failure {
    failure(
        "host_picker_unavailable",
        "Check the local X11 display, authorization, window focus and keyboard mapping, then make a new explicit selection. No remote connection was opened.",
    )
}
#[cfg(test)]
#[path = "host_choice/tests.rs"]
mod tests;
