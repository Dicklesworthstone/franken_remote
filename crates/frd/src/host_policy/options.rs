//! Strict local command parsing and startup resolution. No filesystem mutation.
use super::{Approval, Change, Error, Policy, Sharing, Store, default_path};
use crate::{
    host_run::{Encoder, EncoderSelection, video},
    native_files::ConflictPolicy,
};
mod files;
use std::path::PathBuf;

/// Effective startup options; overrides never modify the saved policy.
#[derive(Debug)]
pub struct Effective {
    pub saved: Policy,
    /// Exact path used by startup resolution; retain it for the live watcher.
    pub policy_path: PathBuf,
    pub approval: Approval,
    pub sharing: Sharing,
    pub port: u16,
}

// Independent, individually validated command-line switches, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct RunOptions {
    pub port: Option<u16>,
    pub socket: Option<PathBuf>,
    pub config: Option<PathBuf>,
    pub approval: Option<Approval>,
    pub sharing: Option<Sharing>,
    pub headless: bool,
    /// Absolute path of the capture worker; defaults next to the frd binary.
    pub worker: Option<PathBuf>,
    /// X11 display to share; defaults to the DISPLAY frd was started with.
    pub display: Option<String>,
    /// Tailscale interface for ingress enforcement; defaults to tailscale0.
    pub interface: Option<String>,
    /// Local PEM CA bundle for the host's own certificate chain.
    pub trust_roots: Option<PathBuf>,
    /// Serve one OS-share lifetime, then exit.
    pub once: bool,
    /// Legacy explicit CPU HEVC developer profile (ADR 0004). Equivalent to
    /// `--encoder software`, but supplying both selectors is refused.
    pub software_explicit: bool,
    /// Local exact backend or explicit hardware-auto selection, never peer input.
    /// Hardware remains experimental until qualified on the actual host.
    pub encoder: Option<EncoderSelection>,
    /// Local maximum capture/codec frame rate; absent retains 30 fps.
    pub fps: Option<u16>,
    /// Local encoder target bits/s; absent retains 8,000,000. Not a hard
    /// instantaneous transport ceiling or an automatic bandwidth estimate.
    pub bitrate: Option<u32>,
    /// Local opt-in to measured raw-capture pacing for a controlling session.
    /// Requires `input_agent`; an observer-first shared source keeps fixed cadence.
    pub adaptive_capture: bool,
    /// Absolute `fr-input-agent` path. Absent: observation only (unchanged).
    /// Present: a first viewer may take the exclusive controlled share.
    pub input_agent: Option<PathBuf>,
    /// Let that controller's text clipboard follow its lease. CLI parsing
    /// enables this with `input_agent` unless `--no-clipboard` is selected;
    /// explicit `--clipboard` without an input agent is refused by the caller.
    /// The separately launched native role requires its own visible Off
    /// surface; an unavailable clipboard keeps the input session usable.
    pub clipboard: bool,
    /// Local enable of host playback audio (off by default). Only an admitted
    /// observer that selected audio-down receives it; see `frd run --help`.
    pub audio: bool,
    /// Absolute local `PulseAudio` server socket for playback capture.
    pub audio_server: Option<PathBuf>,
    /// Sink whose monitor is captured; default: the server's default sink.
    pub audio_sink: Option<String>,
    /// Separate local enable for the controller's microphone virtual source.
    pub microphone: bool,
    /// Exact `PipeWire` socket; absent resolves below the local runtime directory.
    pub microphone_server: Option<PathBuf>,
    /// `--files DIR`: the controller's drop directory (needs `input_agent`;
    /// the caller validates it by type before any I/O).
    pub files: Option<PathBuf>,
    /// Explicit host source files or portable directory roots for one control lease.
    pub send: Vec<PathBuf>,
    /// Opt-in same-user command endpoint for later selections on one original
    /// controlled session. Requires `once` and an input agent.
    pub send_control: Option<String>,
    /// Local collision handling for both files and whole trees. Absent: reject.
    pub files_conflict: Option<ConflictPolicy>,
    /// Per-file byte limit for `--files` (default in `native_files`).
    pub files_max_file_bytes: Option<u64>,
    /// Per-controlled-session cumulative byte limit for `--files`.
    pub files_max_session_bytes: Option<u64>,
    /// `--logind-session ID`: the operator's explicitly selected local logind
    /// session whose lock, logout, switch or suspend ends sharing (plan 2.2).
    pub logind_session: Option<String>,
    /// `--logind-seat SEAT` for that session (default `seat0`).
    pub logind_seat: Option<String>,
    /// Absolute `fr-session-monitor` image (default: next to frd).
    pub session_monitor: Option<PathBuf>,
    /// Absolute read-only Stop sharing image override. macOS requires this
    /// helper for every host run and defaults next to frd. Linux selects it
    /// explicitly for observation-only hosts, including optional audio.
    pub observation_indicator: Option<PathBuf>,
}
impl RunOptions {
    /// Arguments after `run`. Unknown, duplicate or valueless options refuse
    /// BEFORE the runtime or `LocalAPI` is opened. Typos cannot disable approval.
    #[allow(clippy::too_many_lines)] // One flat flag table; splitting it hides duplicate/conflict checks.
    pub fn parse(args: &[String]) -> Result<Self, Error> {
        let mut options = Self::default();
        let mut iter = args.iter();
        let mut json = false;
        let mut clipboard = None;
        while let Some(flag) = iter.next() {
            if flag == "--json" {
                if json {
                    return Err(Error::InvalidArgument);
                }
                json = true;
                continue;
            }
            if flag == "--headless" {
                if options.headless {
                    return Err(Error::InvalidArgument);
                }
                options.headless = true;
                continue;
            }
            if flag == "--software-explicit" {
                if options.software_explicit {
                    return Err(Error::InvalidArgument);
                }
                options.software_explicit = true;
                continue;
            }
            if flag == "--once" {
                if options.once {
                    return Err(Error::InvalidArgument);
                }
                options.once = true;
                continue;
            }
            if flag == "--clipboard" || flag == "--no-clipboard" {
                set(&mut clipboard, flag == "--clipboard")?;
                continue;
            }
            if flag == "--adaptive-capture" {
                if options.adaptive_capture {
                    return Err(Error::InvalidArgument);
                }
                options.adaptive_capture = true;
                continue;
            }
            if flag == "--audio" {
                if options.audio {
                    return Err(Error::InvalidArgument);
                }
                options.audio = true;
                continue;
            }
            if flag == "--microphone" {
                if options.microphone {
                    return Err(Error::InvalidArgument);
                }
                options.microphone = true;
                continue;
            }
            let value = value(&mut iter)?;
            match flag.as_str() {
                "--port" => {
                    let port = value.parse::<u16>().map_err(|_| Error::InvalidArgument)?;
                    if port == 0 {
                        return Err(Error::InvalidArgument);
                    }
                    set(&mut options.port, port)?;
                }
                "--socket" => set(&mut options.socket, PathBuf::from(value))?,
                "--config" => set(&mut options.config, PathBuf::from(value))?,
                "--approval" => set(&mut options.approval, Approval::parse(value)?)?,
                "--sharing" => set(&mut options.sharing, Sharing::parse(value)?)?,
                "--worker" => set(&mut options.worker, PathBuf::from(value))?,
                "--display" => set(&mut options.display, value.to_owned())?,
                "--interface" => set(&mut options.interface, value.to_owned())?,
                "--trust-roots" => set(&mut options.trust_roots, PathBuf::from(value))?,
                "--encoder" => set(
                    &mut options.encoder,
                    EncoderSelection::parse(value).ok_or(Error::InvalidArgument)?,
                )?,
                "--fps" => set(
                    &mut options.fps,
                    u16::try_from(bytes(value)?).map_err(|_| Error::InvalidArgument)?,
                )?,
                "--bitrate" => set(
                    &mut options.bitrate,
                    u32::try_from(bytes(value)?).map_err(|_| Error::InvalidArgument)?,
                )?,
                "--input-agent" => set(&mut options.input_agent, PathBuf::from(value))?,
                "--audio-server" => set(&mut options.audio_server, PathBuf::from(value))?,
                "--microphone-server" => set(&mut options.microphone_server, PathBuf::from(value))?,
                "--audio-sink" => set(&mut options.audio_sink, value.to_owned())?,
                "--files" => set(&mut options.files, PathBuf::from(value))?,
                "--send" => options.send.push(PathBuf::from(value)),
                "--send-control" => set(&mut options.send_control, value.to_owned())?,
                "--files-conflict" => {
                    set(&mut options.files_conflict, files::parse_conflict(value)?)?;
                }
                "--files-max-file-bytes" => set(&mut options.files_max_file_bytes, bytes(value)?)?,
                "--files-max-session-bytes" => {
                    set(&mut options.files_max_session_bytes, bytes(value)?)?;
                }
                "--logind-session" => set(&mut options.logind_session, value.to_owned())?,
                "--logind-seat" => set(&mut options.logind_seat, value.to_owned())?,
                "--session-monitor" => set(&mut options.session_monitor, PathBuf::from(value))?,
                "--observation-indicator" => {
                    let path = PathBuf::from(value);
                    if !path.is_absolute() {
                        return Err(Error::InvalidArgument);
                    }
                    set(&mut options.observation_indicator, path)?;
                }
                _ => return Err(Error::InvalidArgument),
            }
        }
        options.clipboard = clipboard.unwrap_or(options.input_agent.is_some());
        let _ = options.selected_encoder()?;
        let _ = options.video_profile()?;
        // macOS requires a host-lifetime Stop sharing surface in addition to
        // the control executor's independent per-lease panel. Linux retains
        // its explicit view-only observation surface.
        if !cfg!(target_os = "macos")
            && options.observation_indicator.is_some()
            && options.input_agent.is_some()
        {
            return Err(Error::InvalidArgument);
        }
        // A private Xvfb belongs to no logind session. Watching some other session
        // would look like lock protection without providing any.
        if options.headless && options.logind_session.is_some() {
            return Err(Error::InvalidArgument);
        }
        // Linux needs the operator's explicit logind selection. On macOS the
        // helper binds the caller's actual graphical login session natively;
        // a helper path does not invent a logind identity for it.
        if options.logind_session.is_none()
            && (options.logind_seat.is_some()
                || (!cfg!(target_os = "macos") && options.session_monitor.is_some()))
        {
            return Err(Error::InvalidArgument);
        }
        if options
            .session_monitor
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(Error::InvalidArgument);
        }
        // Audio selection flags without the enable are a typo, never a default.
        if !options.audio && (options.audio_server.is_some() || options.audio_sink.is_some()) {
            return Err(Error::InvalidArgument);
        }
        if (options.microphone && options.input_agent.is_none())
            || (!options.microphone && options.microphone_server.is_some())
            || options
                .microphone_server
                .as_ref()
                .is_some_and(|path| !path.is_absolute())
        {
            return Err(Error::InvalidArgument);
        }
        // File limits or conflict choices cannot implicitly enable receiving.
        let _ = options.file_conflict_policy()?;
        options.validate_file_send()?;
        Ok(options)
    }
    /// Resolve only explicit operator input; absence never selects hardware or
    /// software implicitly. Recheck contradictory programmatically built options.
    pub fn selected_encoder(&self) -> Result<Option<EncoderSelection>, Error> {
        match (self.encoder, self.software_explicit) {
            (Some(_), true) => Err(Error::InvalidArgument),
            (Some(encoder), false) => Ok(Some(encoder)),
            (None, true) => Ok(Some(Encoder::SoftwareExplicit.into())),
            (None, false) => Ok(None),
        }
    }
    /// Resolve the same validated settings used by the capture scheduler and
    /// native encoder. Rates do not imply an encoder or enable any authority.
    pub fn video_profile(&self) -> Result<video::Profile, Error> {
        video::Profile::new(
            self.fps.unwrap_or(video::DEFAULT_FPS),
            self.bitrate.unwrap_or(video::DEFAULT_BITRATE),
        )
        .map_err(|_| Error::InvalidArgument)
    }
    pub fn resolve(&self) -> Result<Effective, Error> {
        let _ = self.file_conflict_policy()?;
        self.validate_file_send()?;
        let _ = self.selected_encoder()?;
        let _ = self.video_profile()?;
        if self.port == Some(0) {
            return Err(Error::InvalidArgument);
        }
        let path = match &self.config {
            Some(p) => p.clone(),
            None => default_path()?,
        };
        let saved = Store::new(&path)?.load()?;
        Ok(Effective {
            saved,
            policy_path: path,
            approval: self.approval.unwrap_or(saved.approval_mode),
            sharing: self.sharing.unwrap_or(saved.sharing_scope),
            port: self.port.unwrap_or(8443),
        })
    }

    /// The first requested capability `frd run` cannot provide on macOS yet,
    /// as a typed refusal `(code, detail)`. `frd run` refuses it before any
    /// I/O, and `frd install` before writing a `LaunchAgent` that would only
    /// refuse at every start.
    #[cfg(target_os = "macos")]
    #[must_use]
    pub fn macos_refusal(&self, approval: Approval) -> Option<(&'static str, &'static str)> {
        let refusals: [(bool, &str, &str); 5] = [
            (
                approval == Approval::Local,
                "local_approval_unavailable",
                "macOS has no local approval prompt yet; share with your own devices using --approval none",
            ),
            (
                self.audio,
                "audio_server_unavailable",
                "--audio has no macOS capture backend yet; omit it to share without audio",
            ),
            (
                self.microphone,
                "microphone_endpoint_unavailable",
                "--microphone has no macOS backend yet",
            ),
            (
                self.logind_session.is_some(),
                "session_monitor_unavailable",
                "--logind-session is a Linux logind selection",
            ),
            (
                self.headless || self.display.is_some(),
                "no_display",
                "macOS shares the logged-in user's display; --headless and --display are X11 options",
            ),
        ];
        refusals
            .into_iter()
            .find_map(|(refused, code, detail)| refused.then_some((code, detail)))
    }
}

pub struct PolicyCommand {
    pub path: PathBuf,
    pub change: Option<Change>,
}
impl PolicyCommand {
    /// Arguments after approval/sharing. `unattended` remains an accepted CLI
    /// alias for `none`; disk serialization has only the canonical spelling.
    pub fn parse(args: &[String], approval: bool) -> Result<Self, Error> {
        let mut iter = args.iter();
        let mut positionals = Vec::new();
        let mut path = None;
        let mut json = false;
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--json" if !json => json = true,
                "--config" => set(&mut path, PathBuf::from(value(&mut iter)?))?,
                other if !other.starts_with('-') && positionals.len() < 2 => {
                    positionals.push(other);
                }
                _ => return Err(Error::InvalidArgument),
            }
        }
        let change = match positionals.as_slice() {
            [] | ["get"] => None,
            ["set", mode] if approval => Some(Change::Approval(Approval::parse(mode)?)),
            ["set", scope] => Some(Change::Sharing(Sharing::parse(scope)?)),
            _ => return Err(Error::InvalidArgument),
        };
        Ok(Self {
            path: match path {
                Some(p) => p,
                None => default_path()?,
            },
            change,
        })
    }
}
fn set<T>(slot: &mut Option<T>, value: T) -> Result<(), Error> {
    if slot.is_some() {
        return Err(Error::InvalidArgument);
    }
    *slot = Some(value);
    Ok(())
}
/// A positive decimal count; no units, signs or separators.
fn bytes(value: &str) -> Result<u64, Error> {
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::InvalidArgument);
    }
    match value.parse::<u64>() {
        Ok(0) | Err(_) => Err(Error::InvalidArgument),
        Ok(n) => Ok(n),
    }
}
fn value<'a>(iter: &mut std::slice::Iter<'a, String>) -> Result<&'a str, Error> {
    iter.next()
        .map(String::as_str)
        .filter(|s| !s.is_empty() && !s.starts_with('-'))
        .ok_or(Error::InvalidArgument)
}

#[cfg(test)]
mod download_tests;
#[cfg(test)]
mod encoder_tests;
#[cfg(test)]
mod video_tests;

#[cfg(test)]
mod tests {
    use super::*;
    fn parse(args: &[&str]) -> Result<RunOptions, Error> {
        RunOptions::parse(&args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>())
    }
    #[test]
    fn input_agent_is_opt_in_single_and_never_valueless() {
        assert_eq!(parse(&["--software-explicit"]).unwrap().input_agent, None);
        let options = parse(&["--input-agent", "/usr/libexec/fr-input-agent"]).unwrap();
        assert_eq!(
            options.input_agent,
            Some(PathBuf::from("/usr/libexec/fr-input-agent"))
        );
        for refused in [
            &["--input-agent"][..],
            &["--input-agent", "--once"][..],
            &["--input-agent", "/a", "--input-agent", "/b"][..],
        ] {
            assert!(
                matches!(parse(refused), Err(Error::InvalidArgument)),
                "{refused:?}"
            );
        }
    }
    #[test]
    fn clipboard_defaults_to_control_and_accepts_one_explicit_choice() {
        assert!(!parse(&["--software-explicit"]).unwrap().clipboard);
        assert!(!parse(&["--no-clipboard"]).unwrap().clipboard);
        // Both native host roles default on only for a controlling share.
        assert!(parse(&["--input-agent", "/a"]).unwrap().clipboard);
        assert!(
            parse(&["--input-agent", "/a", "--clipboard"])
                .unwrap()
                .clipboard
        );
        assert!(
            !parse(&["--input-agent", "/a", "--no-clipboard"])
                .unwrap()
                .clipboard
        );
        // Resolve defaults after the complete parse, independent of flag order.
        assert!(
            !parse(&["--no-clipboard", "--input-agent", "/a"])
                .unwrap()
                .clipboard
        );
        // Retain the caller's specific clipboard_requires_input_agent refusal.
        assert!(parse(&["--clipboard"]).unwrap().clipboard);
        for refused in [
            &["--clipboard", "--clipboard"][..],
            &["--no-clipboard", "--no-clipboard"][..],
            &["--clipboard", "--no-clipboard"][..],
            &["--no-clipboard", "--clipboard"][..],
            &["--clipboard=yes"][..],
            &["--no-clipboard=yes"][..],
            &["--input-agent", "--clipboard"][..],
        ] {
            assert!(
                matches!(parse(refused), Err(Error::InvalidArgument)),
                "{refused:?}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_accepts_default_clipboard_without_relaxing_other_capability_refusals() {
        let approval = Approval::parse("none").unwrap();
        let options = parse(&["--input-agent", "/a"]).unwrap();
        assert!(options.clipboard);
        assert_eq!(options.macos_refusal(approval), None);
        assert_eq!(
            options.macos_refusal(Approval::Local).map(|(code, _)| code),
            Some("local_approval_unavailable")
        );
        let options = parse(&["--input-agent", "/a", "--audio"]).unwrap();
        assert_eq!(
            options.macos_refusal(approval).map(|(code, _)| code),
            Some("audio_server_unavailable")
        );
    }

    #[test]
    fn audio_is_an_explicit_local_enable_with_optional_local_selection() {
        let off = parse(&["--software-explicit"]).unwrap();
        assert!(!off.audio && off.audio_server.is_none() && off.audio_sink.is_none());
        let on = parse(&[
            "--audio",
            "--audio-server",
            "/run/user/1000/pulse/native",
            "--audio-sink",
            "speakers",
        ])
        .unwrap();
        assert!(on.audio);
        assert_eq!(
            on.audio_server,
            Some(PathBuf::from("/run/user/1000/pulse/native"))
        );
        assert_eq!(on.audio_sink.as_deref(), Some("speakers"));
        for refused in [
            &["--audio", "--audio"][..],
            &["--audio-server", "/run/native"][..],
            &["--audio-sink", "speakers"][..],
            &["--audio", "--audio-sink"][..],
            &["--audio", "--audio-server", "/a", "--audio-server", "/b"][..],
        ] {
            assert!(
                matches!(parse(refused), Err(Error::InvalidArgument)),
                "{refused:?}"
            );
        }
    }
    #[test]
    fn files_is_an_explicit_local_directory_with_optional_byte_limits() {
        let off = parse(&["--software-explicit"]).unwrap();
        assert!(off.files.is_none() && off.files_max_file_bytes.is_none());
        let on = parse(&[
            "--input-agent",
            "/a",
            "--files",
            "/srv/drop",
            "--files-max-file-bytes",
            "1048576",
            "--files-max-session-bytes",
            "4194304",
        ])
        .unwrap();
        assert_eq!(on.files, Some(PathBuf::from("/srv/drop")));
        assert_eq!(on.files_max_file_bytes, Some(1_048_576));
        assert_eq!(on.files_max_session_bytes, Some(4_194_304));
        for refused in [
            &["--files"][..],
            &["--files", "/a", "--files", "/b"][..],
            &["--files-max-file-bytes", "10"][..],
            &["--files-max-session-bytes", "10"][..],
            &["--files", "/a", "--files-max-file-bytes", "0"][..],
            &["--files", "/a", "--files-max-file-bytes", "1MiB"][..],
            &["--files", "/a", "--files-max-session-bytes", "+5"][..],
        ] {
            assert!(
                matches!(parse(refused), Err(Error::InvalidArgument)),
                "{refused:?}"
            );
        }
    }
    #[test]
    fn session_lifetime_is_an_explicit_local_selection() {
        let off = parse(&["--software-explicit"]).unwrap();
        assert!(off.logind_session.is_none() && off.session_monitor.is_none());
        assert!(
            parse(&["--software-explicit", "--headless"])
                .unwrap()
                .headless
        );
        assert!(matches!(
            parse(&[
                "--software-explicit",
                "--headless",
                "--logind-session",
                "c2"
            ]),
            Err(Error::InvalidArgument)
        ));
        let on = parse(&[
            "--logind-session",
            "c2",
            "--logind-seat",
            "seat1",
            "--session-monitor",
            "/usr/libexec/fr-session-monitor",
        ])
        .unwrap();
        assert_eq!(on.logind_session.as_deref(), Some("c2"));
        assert_eq!(on.logind_seat.as_deref(), Some("seat1"));
        assert_eq!(
            on.session_monitor,
            Some(PathBuf::from("/usr/libexec/fr-session-monitor"))
        );
        for refused in [
            &["--logind-session"][..],
            &["--logind-session", "c2", "--logind-session", "c3"][..],
            &["--logind-seat", "seat0"][..],
            &["--session-monitor", "relative-monitor"][..],
        ] {
            assert!(
                matches!(parse(refused), Err(Error::InvalidArgument)),
                "{refused:?}"
            );
        }
        let native = parse(&["--session-monitor", "/usr/libexec/fr-session-monitor"]);
        if cfg!(target_os = "macos") {
            assert_eq!(
                native.unwrap().session_monitor,
                Some(PathBuf::from("/usr/libexec/fr-session-monitor"))
            );
        } else {
            assert!(matches!(native, Err(Error::InvalidArgument)));
        }
    }
    #[test]
    fn observation_indicator_is_explicit_absolute_and_cannot_change_approval() {
        assert!(parse(&[]).unwrap().observation_indicator.is_none());
        let on = parse(&[
            "--observation-indicator",
            "/usr/libexec/fr-observation-indicator",
            "--approval",
            "local",
            "--audio",
        ])
        .unwrap();
        assert_eq!(on.approval, Some(Approval::Local));
        assert!(on.audio && on.input_agent.is_none());
        assert_eq!(
            on.observation_indicator,
            Some(PathBuf::from("/usr/libexec/fr-observation-indicator"))
        );
        for refused in [
            &["--observation-indicator"][..],
            &["--observation-indicator", "--once"][..],
            &["--observation-indicator", "relative"][..],
            &[
                "--observation-indicator",
                "/a",
                "--observation-indicator",
                "/b",
            ][..],
        ] {
            assert!(
                matches!(parse(refused), Err(Error::InvalidArgument)),
                "{refused:?}"
            );
        }
        let controlled = parse(&["--observation-indicator", "/a", "--input-agent", "/b"]);
        if cfg!(target_os = "macos") {
            let controlled = controlled.unwrap();
            assert_eq!(controlled.observation_indicator, Some(PathBuf::from("/a")));
            assert_eq!(controlled.input_agent, Some(PathBuf::from("/b")));
            assert!(controlled.clipboard);
        } else {
            assert!(matches!(controlled, Err(Error::InvalidArgument)));
        }
    }
}

#[cfg(test)]
#[path = "options/microphone_tests.rs"]
mod microphone_tests;
