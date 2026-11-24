//! `frd run` on Windows (fr-p2-host-windows-rt1). The shared desktop is the
//! interactive one, read by the Windows capture worker (Desktop Duplication
//! into Media Foundation HEVC), and identity comes from tailscaled's named
//! pipe. Options whose backends exist only on Linux or macOS so far are typed
//! refusals before any I/O, never ignored. Observation and control can both
//! require local native consent on the original monitored logon session.
// These keep the Linux functions' signatures so the shared run calls either.
#![allow(clippy::unnecessary_wraps)]

use super::{SelectedDesktop, run_refusal};
use frd::host_policy::{Approval, options::RunOptions};
use std::process::ExitCode;

/// Refuse every requested capability this host cannot provide on Windows yet.
pub(super) fn supported(
    options: &RunOptions,
    approval: Approval,
    json: bool,
) -> Result<(), ExitCode> {
    match options.windows_refusal(approval) {
        Some((code, detail)) => Err(run_refusal(json, code, detail, 2)),
        None => Ok(()),
    }
}

/// The interactive desktop of this session; no X11 name, cookie or Xvfb.
pub(super) fn select_desktop(
    _options: &RunOptions,
    _json: bool,
) -> Result<SelectedDesktop, ExitCode> {
    Ok((String::new(), None, None))
}

/// The logon session this process runs in, always watched: locking it,
/// switching away from it or logging off ends a share. No helper image.
pub(super) fn session_monitor(
    _options: &RunOptions,
    _display: &str,
    json: bool,
) -> Result<Option<frd::session_monitor::Configuration>, ExitCode> {
    use frd::session_monitor::{Configuration, Selection};
    match Selection::windows() {
        Ok(selection) => Ok(Some(Configuration {
            image: std::path::PathBuf::new(),
            selection,
        })),
        Err(_) => Err(run_refusal(
            json,
            "session_monitor_unavailable",
            "this process's Windows logon session could not be read",
            2,
        )),
    }
}

/// Explicit endpoint-wide playback authorization. The demand-driven worker
/// starts only for an admitted viewer that positively negotiated AudioDown.
pub(super) fn audio(
    options: &RunOptions,
    json: bool,
) -> Result<Option<frd::host_run::AudioOptions>, ExitCode> {
    // Keep the consent gate here as well: a programmatically constructed
    // request cannot bypass the earlier command/platform validation.
    if let Some((code, detail)) = options.windows_refusal(Approval::None) {
        return Err(run_refusal(json, code, detail, 2));
    }
    Ok(options
        .audio
        .then_some(frd::host_run::AudioOptions::WindowsEndpoint))
}

/// No microphone endpoint yet; `supported` refused `--microphone`.
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
    fn playback_requires_endpoint_scope_without_enabling_other_capabilities() {
        assert_eq!(audio(&RunOptions::default(), false).unwrap(), None);
        let parse = |args: &[&str]| {
            RunOptions::parse(&args.iter().map(|v| (*v).to_owned()).collect::<Vec<_>>()).unwrap()
        };
        assert!(audio(&parse(&["--audio"]), false).is_err());
        let options = parse(&["--audio", "--audio-endpoint-wide"]);
        assert_eq!(
            audio(&options, false).unwrap(),
            Some(AudioOptions::WindowsEndpoint)
        );
        assert!(!options.control && !options.clipboard && !options.microphone);
        assert!(options.input_agent.is_none() && options.files.is_none());
        for args in [
            &[
                "--audio",
                "--audio-endpoint-wide",
                "--audio-server",
                r"C:\\pulse",
            ][..],
            &[
                "--audio",
                "--audio-endpoint-wide",
                "--audio-sink",
                "speakers",
            ][..],
        ] {
            let options = parse(args);
            assert_eq!(
                options
                    .windows_refusal(Approval::None)
                    .map(|(code, _)| code),
                Some("audio_source_unsupported")
            );
            assert!(audio(&options, false).is_err());
        }
    }
}
