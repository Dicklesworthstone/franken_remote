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

/// Every macOS host keeps a local, visible Stop sharing surface, including
/// view-only and idle listening. The controller's own per-lease indicator is
/// independent. A missing package helper never becomes invisible hosting.
pub(super) fn observation_indicator(
    options: &RunOptions,
    json: bool,
) -> Result<std::path::PathBuf, ExitCode> {
    let image = options.observation_indicator.clone().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("fr-observation-indicator")))
    });
    image
        .filter(|path| path.is_absolute() && path.is_file())
        .ok_or_else(|| {
            run_refusal(
                json,
                "observation_indicator_unavailable",
                "fr-observation-indicator not found; build fr-native with --features \
                 macos-observation-indicator and install it beside frd or pass \
                 --observation-indicator /absolute/path/fr-observation-indicator",
                2,
            )
        })
}

/// Always bind the actual graphical login session before serving. A missing
/// helper is a startup refusal, never a silently unmonitored host. This public
/// native evidence covers console identity and sleep; it is not qualified
/// screen-lock evidence (reported separately in the listening profile).
pub(super) fn session_monitor(
    options: &RunOptions,
    display: &str,
    json: bool,
) -> Result<Option<frd::session_monitor::Configuration>, ExitCode> {
    use frd::session_monitor::{Configuration, Selection};

    if !display.is_empty() {
        return Err(run_refusal(
            json,
            "invalid_session_selection",
            "macOS has no X11 session selection",
            2,
        ));
    }
    let image = options.session_monitor.clone().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("fr-session-monitor")))
    });
    let Some(image) = image.filter(|path| path.is_absolute() && path.is_file()) else {
        return Err(run_refusal(
            json,
            "session_monitor_unavailable",
            "fr-session-monitor not found; build fr-native with --features macos-session-monitor \
             and install it beside frd or pass --session-monitor /absolute/path/fr-session-monitor",
            2,
        ));
    };
    Ok(Some(Configuration {
        image,
        selection: Selection::macos(rustix::process::geteuid().as_raw()),
    }))
}

/// Local application/system playback enable. Capture still starts only on
/// demand from an admitted viewer that negotiated audio-down.
pub(super) fn audio(
    options: &RunOptions,
    json: bool,
) -> Result<Option<frd::host_run::AudioOptions>, ExitCode> {
    if !options.audio {
        return Ok(None);
    }
    if options.audio_server.is_some() || options.audio_sink.is_some() {
        return Err(run_refusal(
            json,
            "audio_source_unsupported",
            "macOS --audio captures application/system playback; --audio-server and --audio-sink are Linux-only selections",
            2,
        ));
    }
    Ok(Some(frd::host_run::AudioOptions::MacSystem))
}

/// Refused in [`supported`].
pub(super) fn microphone(
    _options: &RunOptions,
    _json: bool,
) -> Result<Option<frd::host_run::MicrophoneOptions>, ExitCode> {
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use frd::host_run::AudioOptions;

    #[test]
    fn playback_scope_is_explicit_and_linux_selections_are_never_reinterpreted() {
        assert_eq!(audio(&RunOptions::default(), false).unwrap(), None);
        let parse = |args: &[&str]| {
            RunOptions::parse(&args.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>()).unwrap()
        };
        let options = parse(&["--audio"]);
        assert_eq!(
            audio(&options, false).unwrap(),
            Some(AudioOptions::MacSystem)
        );
        assert!(options.input_agent.is_none() && !options.microphone);
        for args in [
            &["--audio", "--audio-server", "/run/pulse/native"][..],
            &["--audio", "--audio-sink", "speakers"][..],
        ] {
            let options = parse(args);
            assert_eq!(
                options.macos_refusal(Approval::None).map(|(code, _)| code),
                Some("audio_source_unsupported")
            );
            assert!(audio(&options, false).is_err());
        }
    }
}
