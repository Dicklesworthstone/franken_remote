//! `frd run` on macOS (fr-p1-host-macos-vif). The shared desktop is the
//! logged-in user's window server, read by the macOS capture worker, and
//! identity comes from the installed Tailscale app's `LocalAPI`. Options whose
//! backends exist only on Linux so far are typed refusals, never ignored.
// These keep the Linux functions' signatures so the shared run calls either.
#![allow(clippy::unnecessary_wraps)]

use super::{SelectedDesktop, run_refusal};
use frd::host_policy::{Approval, options::RunOptions};
use std::process::ExitCode;

/// Refuse every requested capability this host cannot provide on macOS yet,
/// before any network, worker or capture I/O.
pub(super) fn supported(
    options: &RunOptions,
    approval: Approval,
    json: bool,
) -> Result<(), ExitCode> {
    let refusals: [(bool, &str, &str); 9] = [
        (
            approval == Approval::Local,
            "local_approval_unavailable",
            "macOS has no local approval prompt yet; share with your own devices using --approval none",
        ),
        (
            options.input_agent.is_some(),
            "input_agent_unavailable",
            "remote control of a Mac is not implemented yet; omit --input-agent to share view-only",
        ),
        (
            options.audio,
            "audio_server_unavailable",
            "--audio has no macOS capture backend yet; omit it to share without audio",
        ),
        (
            options.microphone,
            "microphone_endpoint_unavailable",
            "--microphone has no macOS backend yet",
        ),
        (
            options.observation_indicator.is_some(),
            "observation_indicator_unavailable",
            "--observation-indicator is an X11 window; it has no macOS backend yet",
        ),
        (
            options.logind_session.is_some(),
            "session_monitor_unavailable",
            "--logind-session is a Linux logind selection",
        ),
        (
            options.headless || options.display.is_some(),
            "no_display",
            "macOS shares the logged-in user's display; --headless and --display are X11 options",
        ),
        (
            options.send_control.is_some(),
            "host_file_commands_unsupported",
            "--send-control uses a Linux-only local socket",
        ),
        (
            options.socket.is_some(),
            "tailscale_unavailable",
            "macOS reads the installed Tailscale app's LocalAPI; --socket is Linux-only",
        ),
    ];
    match refusals.iter().find(|(refused, _, _)| *refused) {
        Some(&(_, code, detail)) => Err(run_refusal(json, code, detail, 2)),
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

/// `--send-control` is refused in [`supported`], so no owner ever exists.
pub(super) mod file_commands_host {
    use super::run_refusal;
    use frd::native_files::HostSelection;
    use std::process::ExitCode;

    pub(in super::super) enum Owner {}

    pub(in super::super) fn open(
        _target: &str,
        json: bool,
    ) -> Result<(HostSelection, Owner), ExitCode> {
        Err(run_refusal(
            json,
            "host_file_commands_unsupported",
            "--send-control uses a Linux-only local socket",
            2,
        ))
    }

    pub(in super::super) fn finish(owner: Option<&Owner>) -> bool {
        match owner {
            None => true,
            Some(owner) => match *owner {},
        }
    }
}
