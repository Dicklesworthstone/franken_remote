//! Nonmodal `AppKit` machine choice before the original connector starts.
//! Discovery rows retain exact identities; selection never retains an address
//! or substitutes for the connector's fresh authenticated lookup.
use super::{Cell, Connection, Cx, Failure, LocalApi, Runtime, Shutdown, failure, tailnet};
use crate::{host_choice, saved_hosts::Selection};
use fr_client::discovery::picker::Candidate;
use fr_native::macos::picker::{Error, HostPicker};
use std::time::Duration;

const POLL: Duration = Duration::from_millis(10);

#[allow(clippy::too_many_arguments)] // The original CLI runtime and signal owner.
pub(super) fn select(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: &LocalApi,
    connection: &Connection,
    json: bool,
) -> Result<Selection, Failure> {
    host_choice::validate(&connection.target)?;
    let snapshot = runtime
        .block_on(shutdown.run(cx, stopped, api.discover(cx)))
        .map_err(tailnet)?;
    if stopped.get() || cx.is_cancel_requested() {
        return Err(host_choice::cancelled());
    }
    let catalog = host_choice::catalog(&snapshot)?;
    if catalog.is_empty() {
        return Err(failure(
            "host_choice_empty",
            "The installed Tailscale daemon returned no selectable peers. Check its account, peer visibility and key expiry; no remote connection was opened.",
        ));
    }
    let mut picker = HostPicker::open(catalog).map_err(picker_failure)?;
    let choice = runtime.block_on(shutdown.run(cx, stopped, wait(cx, &mut picker)));
    // Retire the exact main-thread native owner even when the operation was
    // cancelled. A choice cannot start a connection before actual UI cleanup.
    picker.close().map_err(|_| {
        failure(
            "host_picker_cleanup_incomplete",
            "The original native host picker has not confirmed cleanup. No remote connection was opened.",
        )
    })?;
    if stopped.get() || cx.is_cancel_requested() {
        return Err(host_choice::cancelled());
    }
    let candidate = choice?;
    let selected = Selection::from_candidate(&candidate, connection.target.port);
    crate::output::write(&host_choice::selected_report(
        &candidate,
        selected.port,
        json,
    ))
    .map_err(|_| {
        Failure::new(
            "output_unavailable",
            "The selected-host report could not be written. No remote connection was opened.",
            74,
        )
    })?;
    Ok(selected)
}

async fn wait(cx: &Cx, picker: &mut HostPicker) -> Result<Candidate, Failure> {
    let clock = cx.timer_driver().ok_or_else(unavailable)?;
    loop {
        if cx.is_cancel_requested() {
            return Err(host_choice::cancelled());
        }
        if let Some(candidate) = picker.poll().map_err(picker_failure)? {
            return Ok(candidate);
        }
        // AppKit is pumped nonmodally in a bounded batch. The same runtime
        // continues to observe the original signal/cancellation owner.
        asupersync::time::sleep(clock.now(), POLL).await;
    }
}

fn picker_failure(error: Error) -> Failure {
    match error {
        Error::Cancelled => host_choice::cancelled(),
        Error::ChoiceTimeout => failure(
            "host_choice_expired",
            "The original host-choice deadline expired. Start a fresh local choice; no remote connection was opened.",
        ),
        Error::EventFlood => failure(
            "host_choice_event_overflow",
            "The local picker could not finish its bounded event batch. No queued choice was accepted and no remote connection was opened.",
        ),
        _ => unavailable(),
    }
}

fn unavailable() -> Failure {
    failure(
        "host_picker_unavailable",
        "The local macOS host picker could not complete a valid choice. Run from the logged-in graphical session and make a fresh selection; no remote connection was opened.",
    )
}
