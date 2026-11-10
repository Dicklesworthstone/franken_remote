//! `frd run` on macOS (fr-p1-host-macos-vif). The shared desktop is the
//! logged-in user's window server, read by the macOS capture worker, and
//! identity comes from the installed Tailscale `LocalAPI` (the app's loopback
//! endpoint, or the open-source tailscaled's root socket). Options whose
//! backends exist only on Linux so far are typed refusals, never ignored.
// These keep the Linux functions' signatures so the shared run calls either.
#![allow(clippy::unnecessary_wraps)]

use super::{SelectedDesktop, run_refusal};
use frd::host_policy::{Approval, options::RunOptions};
use std::process::ExitCode;

/// Refuse every requested capability this host cannot provide on macOS yet,
/// before any network, worker or capture I/O (the table is shared with
/// `frd install`).
pub(super) fn supported(
    options: &RunOptions,
    approval: Approval,
    json: bool,
) -> Result<(), ExitCode> {
    match options.macos_refusal(approval) {
        Some((code, detail)) => Err(run_refusal(json, code, detail, 2)),
        None => Ok(()),
    }
}

/// The window server of this login session; no X11 name, cookie or Xvfb.
pub(super) fn select_desktop(
    _options: &RunOptions,
    _json: bool,
) -> Result<SelectedDesktop, ExitCode> {
    Ok((String::new(), None, None))
}

/// Refused in [`supported`]; the session lifetime is not observed on macOS.
pub(super) fn session_monitor(
    _options: &RunOptions,
    _display: &str,
    _json: bool,
) -> Result<Option<frd::session_monitor::Configuration>, ExitCode> {
    Ok(None)
}

/// Refused in [`supported`].
pub(super) fn audio(
    _options: &RunOptions,
    _json: bool,
) -> Result<Option<frd::host_run::AudioOptions>, ExitCode> {
    Ok(None)
}

/// Refused in [`supported`].
pub(super) fn microphone(
    _options: &RunOptions,
    _json: bool,
) -> Result<Option<frd::host_run::MicrophoneOptions>, ExitCode> {
    Ok(None)
}
