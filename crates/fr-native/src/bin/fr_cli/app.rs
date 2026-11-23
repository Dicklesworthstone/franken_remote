//! `fr app`: the desktop application on macOS and Linux, the counterpart of
//! the Windows `FrankenRemote.exe`. It loops over two child commands of this
//! same executable, so every connection takes the ordinary tested path:
//!
//! 1. `fr connect --choose-host --pick-only --json ...` shows the native
//!    machine picker with the connection choices (view only or view and
//!    control, clipboard text, which display, host audio, one-use file sharing)
//!    and the previous outcome, then
//!    reports the chosen identity and choices. Explicit `--probe` adds bounded
//!    capability inspection before the picker; it opens no desktop observation.
//! 2. `fr connect NODE ... --json` is the plain viewer for that choice.
//!
//! The application keeps only local preferences (the same document as the
//! Windows app). It never holds a peer address, lease, or remote session, and
//! the second command re-resolves the chosen identity before dialing.
//!
//! `fr app --install` adds a launcher that runs `fr app`:
//! `~/Applications/FrankenRemote.app` on macOS, and an XDG desktop entry
//! (`~/.local/share/applications/frankenremote.desktop`) on Linux.
use crate::{Failure, options::MAX_APP_NOTICE};
use fr_client::completion::CompletionCapture;
use fr_client::discovery::DEFAULT_SERVICE_PORT;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[path = "app/child.rs"]
mod child;
use child::Finished;
#[path = "app/receiving.rs"]
mod receiving;
use receiving::Receiving;
#[path = "app/microphone.rs"]
mod microphone;
use microphone::Microphone;
#[cfg(test)]
#[path = "app/microphone_app_tests.rs"]
mod microphone_app_tests;
#[path = "app/receive_completion.rs"]
mod receive_completion;
#[cfg(test)]
#[path = "app/receive_tests.rs"]
mod receive_tests;
#[cfg(test)]
#[path = "app/receiving_tests.rs"]
mod receiving_tests;
#[path = "app/upload_completion.rs"]
mod upload_completion;

const USAGE: &str = "fr app [--probe] [--port PORT] [--receive DIR [RECEIVE_OPTIONS]] | --install | --uninstall\n\n\
Without options, opens the machine picker: choose a machine on your tailnet,\n\
View only or View and control, Share clipboard text, Ask which display to\n\
show and Play host audio, then Connect. Audio starts off; enabling it never\n\
enables your microphone. Show microphone controls is available with a local\n\
microphone build (on Linux, also select a source at launch); it\n\
opens the muted panel for this connection only; choose Allow microphone, then\n\
Talk there. This choice starts off in every picker. With control, select Send\n\
files to allow file drops\n\
onto this viewer only. On Linux or macOS, select Receive files to choose a private\n\
folder before connecting. Both choices start off in every new picker. Closing the\n\
viewer returns to the picker with the outcome; Cancel or Escape quits. Viewing\n\
preferences are remembered. An unused\n\
picker reopens with a fresh machine list every two minutes; after half an\n\
hour unused, fr app quits.\n\n\
--port PORT  connect to this service port for this app run (default 8443);\n\
             applies to every choice and is not saved in preferences.\n\
--probe      check up to 64 discovered hosts before each picker (20 s maximum).\n\
             Rows show authenticated capability advertisements or probe failures;\n\
             desktop readiness and permission are checked when connecting.\n\
             No observation or input is opened by a probe. All rows stay selectable.\n\
--receive DIR  receive host-selected files in this existing private directory\n\
               on the next selected controlling connection only. Select Receive files; it is not\n\
               remembered, repeated after any choice, or used by view-only.\n\
--receive-max-file-bytes N     optional positive per-file receive limit.\n\
--receive-max-session-bytes N  optional positive total receive limit.\n\
--receive-conflict POLICY      reject (default) or keep-both; never overwrite.\n\
--microphone-source NAME  use this explicit local source on the next selected\n\
               connection only; choose Show microphone controls, then Allow and Talk.\n\
               Linux requires a named PulseAudio source. macOS can pin default.\n\
--microphone-server PATH  optional local PulseAudio socket on Linux.\n\
--install    add FrankenRemote to ~/Applications (macOS) or the desktop\n\
             applications menu (Linux); it runs this fr with `app`.\n\
--uninstall  remove that launcher (nothing else).\n";
/// Settings are a small local document; anything larger is not ours.
const MAX_SETTINGS_BYTES: u64 = 4096;
/// Stable IDs from installed Tailscale are short printable tokens.
const MAX_ID_BYTES: usize = 128;
/// Each picker choice expires after two minutes; 15 unused ones in a row
/// (half an hour) end the application.
const MAX_IDLE_EXPIRIES: u8 = 15;
/// Marks a desktop entry or bundle this command wrote, so `--uninstall`
/// never removes someone else's file of the same name.
#[cfg(any(not(target_os = "macos"), test))]
const DESKTOP_MARKER: &str = "X-FrankenRemote-Launcher=1";
#[cfg(target_os = "macos")]
const BUNDLE_ID: &str = "com.frankenremote.launcher";

pub(super) fn requested(args: &[String]) -> bool {
    args.first().is_some_and(|first| first == "app")
}

pub(super) fn run(args: &[String]) -> Result<String, Failure> {
    match app_command(args)? {
        AppCommand::Open {
            port,
            receiving,
            microphone,
            probe,
        } => application(port, receiving, microphone, probe),
        AppCommand::Help => Ok(USAGE.to_owned()),
        AppCommand::Install => install(),
        AppCommand::Uninstall => uninstall(),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AppCommand {
    Open {
        port: u16,
        receiving: Option<Receiving>,
        microphone: Option<Microphone>,
        probe: bool,
    },
    Help,
    Install,
    Uninstall,
}

/// Validate the whole invocation before touching settings, discovery or a
/// launcher. The selected endpoint belongs to this run, never to a saved
/// preference or a child report.
fn app_command(args: &[String]) -> Result<AppCommand, Failure> {
    let invalid = || Failure::new("invalid_arguments", "Run fr app --help.", 2);
    if !requested(args)
        || args.len() > 32
        || args
            .iter()
            .any(|arg| arg.len() > 4096 || arg.as_bytes().contains(&0))
    {
        return Err(invalid());
    }
    let mut port = None;
    let mut action = None;
    let mut receive_args = Vec::new();
    let mut microphone_args = Vec::new();
    let mut microphone_source = false;
    let mut microphone_server = false;
    let mut json = false;
    let mut probe = false;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" if !json => json = true,
            "--probe" if !probe => probe = true,
            "--port" if port.is_none() => {
                let raw = rest.next().ok_or_else(invalid)?;
                if raw.is_empty() || raw.len() > 5 || !raw.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(invalid());
                }
                port = Some(
                    raw.parse::<u16>()
                        .ok()
                        .filter(|p| *p != 0)
                        .ok_or_else(invalid)?,
                );
            }
            "--microphone-source" if !microphone_source => {
                microphone_source = true;
                microphone_args.push(arg.clone());
                microphone_args.push(rest.next().ok_or_else(invalid)?.clone());
            }
            "--microphone-server" if !microphone_server => {
                microphone_server = true;
                microphone_args.push(arg.clone());
                microphone_args.push(rest.next().ok_or_else(invalid)?.clone());
            }
            "--receive"
            | "--receive-max-file-bytes"
            | "--receive-max-session-bytes"
            | "--receive-conflict" => {
                receive_args.push(arg.clone());
                receive_args.push(rest.next().ok_or_else(invalid)?.clone());
            }
            "--help" | "-h" if action.is_none() => action = Some(AppCommand::Help),
            "--install" if action.is_none() => action = Some(AppCommand::Install),
            "--uninstall" if action.is_none() => action = Some(AppCommand::Uninstall),
            _ => return Err(invalid()),
        }
    }
    if action.is_some() && (!receive_args.is_empty() || !microphone_args.is_empty() || probe) {
        return Err(invalid());
    }
    let receiving = Receiving::parse(receive_args)?;
    let microphone = Microphone::parse(microphone_args)?;
    match (action, port) {
        (None, port) => Ok(AppCommand::Open {
            port: port.unwrap_or(DEFAULT_SERVICE_PORT),
            receiving,
            microphone,
            probe,
        }),
        (Some(action), None) => Ok(action),
        (Some(_), Some(_)) => Err(invalid()),
    }
}

/// The remembered choices; the same document as the Windows application's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent local connection preferences"
)]
struct Settings {
    control: bool,
    clipboard: bool,
    choose_display: bool,
    playback_audio: bool,
    last_machine: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            control: true,
            clipboard: true,
            choose_display: false,
            playback_audio: false,
            last_machine: None,
        }
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= MAX_ID_BYTES && id.bytes().all(|b| b.is_ascii_graphic())
}

impl Settings {
    /// Unknown, malformed or wrongly typed fields keep their defaults.
    fn from_json(bytes: &[u8]) -> Self {
        let mut settings = Self::default();
        let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(bytes) else {
            return settings;
        };
        if map.get("version").and_then(Value::as_u64) != Some(1) {
            return settings;
        }
        let flag =
            |name: &str, default: bool| map.get(name).and_then(Value::as_bool).unwrap_or(default);
        settings.control = flag("control", settings.control);
        settings.clipboard = flag("clipboard", settings.clipboard);
        settings.choose_display = flag("choose_display", settings.choose_display);
        settings.playback_audio = flag("playback_audio", settings.playback_audio);
        settings.last_machine = map
            .get("last_machine")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .map(str::to_owned);
        settings
    }

    fn to_json(&self) -> String {
        let mut document = json!({
            "version": 1,
            "control": self.control,
            "clipboard": self.clipboard,
            "choose_display": self.choose_display,
            "playback_audio": self.playback_audio,
        });
        if let Some(id) = self.last_machine.as_deref().filter(|id| valid_id(id)) {
            document["last_machine"] = json!(id);
        }
        let mut text = serde_json::to_string_pretty(&document).unwrap_or_default();
        text.push('\n');
        text
    }
}

fn home() -> Result<PathBuf, Failure> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or(Failure::new(
            "home_unavailable",
            "Set HOME to your absolute home directory; fr app keeps its choices and launcher there.",
            2,
        ))
}

fn absolute_env(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
}

#[cfg(target_os = "macos")]
fn settings_path(home: &Path) -> PathBuf {
    home.join("Library/Application Support/FrankenRemote/settings.json")
}

#[cfg(not(target_os = "macos"))]
fn settings_path(home: &Path) -> PathBuf {
    absolute_env("XDG_CONFIG_HOME")
        .unwrap_or_else(|| home.join(".config"))
        .join("frankenremote/settings.json")
}

fn load(path: &Path) -> Settings {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return Settings::default();
    };
    let mut bytes = Vec::new();
    if file
        .take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_SETTINGS_BYTES
    {
        return Settings::default();
    }
    Settings::from_json(&bytes)
}

/// Write-then-rename, so a crash never leaves a torn document. Failing to
/// remember a choice never blocks the connection.
fn save(path: &Path, settings: &Settings) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let staged = path.with_extension("json.new");
    if std::fs::write(&staged, settings.to_json()).is_ok()
        && std::fs::rename(&staged, path).is_err()
    {
        let _ = std::fs::remove_file(&staged);
    }
}

/// One child command's result, from its complete final nonempty line only.
#[derive(Debug, PartialEq, Eq)]
enum Report {
    Selected {
        node: String,
        name: String,
        port: u16,
        control: bool,
        clipboard: bool,
        choose_display: bool,
        playback_audio: bool,
        send_files: bool,
        receive_files: bool,
        microphone_controls: bool,
    },
    Failed {
        code: String,
        next: String,
        unknown_effect: bool,
    },
    /// A native session end with positive local cleanup evidence.
    Ended,
}

fn last_report(stdout: &CompletionCapture) -> Option<Report> {
    let value = serde_json::from_str::<Value>(stdout.completion().ok()?).ok()?;
    if let Some(error) = value.get("error") {
        if value.get("schema_version").and_then(Value::as_u64) != Some(1) {
            return None;
        }
        let unknown_effect = match value.get("outcome").and_then(Value::as_str)? {
            "unknown_external_effect" => true,
            "refused" | "cancelled" | "revoked" => false,
            _ => return None,
        };
        return Some(Report::Failed {
            code: error
                .get("code")?
                .as_str()
                .filter(|code| valid_id(code))?
                .to_owned(),
            next: error.get("next_action")?.as_str()?.to_owned(),
            unknown_effect,
        });
    }
    if value.get("event").and_then(Value::as_str) == Some("host_selected") {
        if value.get("schema_version").and_then(Value::as_u64) != Some(1)
            || value.get("selection_only").and_then(Value::as_bool) != Some(true)
            || value.get("fresh_lookup_required").and_then(Value::as_bool) != Some(true)
            || value.get("remote_session_started").and_then(Value::as_bool) != Some(false)
            || value.get("input_requested").and_then(Value::as_bool) != Some(false)
        {
            return None;
        }
        let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
        let flag = |name: &str| value.get(name).and_then(Value::as_bool);
        let (
            Some(node),
            Some(name),
            Some(control),
            Some(clipboard),
            Some(choose_display),
            Some(playback_audio),
            Some(send_files),
            Some(receive_files),
            Some(microphone_controls),
        ) = (
            text("node_id").filter(|id| valid_id(id)),
            text("certificate_name"),
            flag("control"),
            flag("clipboard"),
            flag("choose_display"),
            flag("playback_audio"),
            flag("send_files"),
            flag("receive_files"),
            flag("microphone_controls"),
        )
        else {
            return None;
        };
        if (send_files || receive_files || microphone_controls) && !control {
            return None;
        }
        return Some(Report::Selected {
            node,
            name,
            port: u16::try_from(value.get("port")?.as_u64()?)
                .ok()
                .filter(|p| *p != 0)?,
            control,
            clipboard,
            choose_display,
            playback_audio,
            send_files,
            receive_files,
            microphone_controls,
        });
    }
    // These are the actual Linux and macOS connect completion formats. A
    // discovery/doctor success, arbitrary JSON, or an unconfirmed cleanup is
    // not a completed desktop session. macOS currently has no schema field.
    let linux_end = value.get("schema_version").and_then(Value::as_u64) == Some(1)
        && value.get("outcome").and_then(Value::as_str) == Some("stopped")
        && matches!(
            value.get("role").and_then(Value::as_str),
            Some("observe" | "control")
        )
        && value.get("cleanup_confirmed").and_then(Value::as_bool) == Some(true);
    let macos_end = value.get("platform").and_then(Value::as_str) == Some("macos")
        && value
            .get("schema_version")
            .is_none_or(|version| version.as_u64() == Some(1))
        && value.get("outcome").and_then(Value::as_str) == Some("success")
        && value
            .get("ended")
            .and_then(Value::as_str)
            .is_some_and(|end| !end.is_empty())
        && value
            .get("local_cleanup_confirmed")
            .and_then(Value::as_bool)
            == Some(true);
    (linux_end || macos_end).then_some(Report::Ended)
}

#[cfg(test)]
fn picker_report(picked: &Finished, expected_port: u16, receive_available: bool) -> Option<Report> {
    picker_report_with_microphone(
        picked,
        expected_port,
        receive_available,
        microphone::available(None),
    )
}

fn picker_report_with_microphone(
    picked: &Finished,
    expected_port: u16,
    receive_available: bool,
    microphone_available: bool,
) -> Option<Report> {
    match last_report(&picked.stdout) {
        Some(Report::Selected {
            port,
            receive_files,
            microphone_controls,
            ..
        }) if !picked.status.success()
            || port != expected_port
            || (receive_files && !receive_available)
            || (microphone_controls && !microphone_available) =>
        {
            None
        }
        report => report,
    }
}

/// Printable ASCII within the pickers' label bound.
fn notice_text(text: &str) -> String {
    let mut clean: String = text
        .chars()
        .map(|c| {
            if c == ' ' || c.is_ascii_graphic() {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if clean.len() > MAX_APP_NOTICE {
        clean.truncate(MAX_APP_NOTICE - 3);
        clean.push_str("...");
    }
    if clean.is_empty() {
        clean.push_str("The last attempt ended.");
    }
    clean
}

fn short_name(certificate_name: &str) -> &str {
    certificate_name
        .split('.')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(certificate_name)
}

fn pick_args(
    settings: &Settings,
    notice: Option<&str>,
    port: u16,
    receive_available: bool,
) -> Vec<String> {
    let port_text = port.to_string();
    let mut args: Vec<String> = [
        "connect",
        "--choose-host",
        "--pick-only",
        "--json",
        "--port",
        &port_text,
        "--experimental-native",
        "--display",
        if settings.choose_display {
            "choose"
        } else {
            "only"
        },
        if settings.control {
            "--control"
        } else {
            "--view-only"
        },
        if settings.clipboard {
            "--clipboard"
        } else {
            "--no-clipboard"
        },
    ]
    .map(str::to_owned)
    .into();
    if settings.playback_audio {
        args.push("--audio".to_owned());
    }
    if receive_available {
        args.push("--app-receive-available".to_owned());
    }
    let port_notice = (port != DEFAULT_SERVICE_PORT).then(|| {
        notice_text(&format!(
            "Service port {port}. {}",
            notice.unwrap_or_default()
        ))
    });
    if let Some(notice) = port_notice.as_deref().or(notice) {
        args.push("--app-notice".to_owned());
        args.push(notice.to_owned());
    }
    args
}

fn session_args(node: &str, settings: &Settings, port: u16, send_files: bool) -> Vec<String> {
    let port_text = port.to_string();
    let mut args: Vec<String> = [
        "connect",
        node,
        "--json",
        "--port",
        &port_text,
        "--experimental-native",
        "--display",
        if settings.choose_display {
            "choose"
        } else {
            "only"
        },
        if settings.control {
            "--control"
        } else {
            "--view-only"
        },
    ]
    .map(str::to_owned)
    .into();
    // Clipboard text follows control by default; view-only never offers it.
    if settings.control && !settings.clipboard {
        args.push("--no-clipboard".to_owned());
    }
    if settings.playback_audio {
        args.push("--audio".to_owned());
    }
    // File sending belongs only to this confirmed controlling connection,
    // never to saved settings or the next picker after a session ends.
    if settings.control && send_files {
        args.push("--file-drop".to_owned());
    }
    args
}

fn probed_pick_args(
    settings: &Settings,
    notice: Option<&str>,
    port: u16,
    receive_available: bool,
    probe: bool,
    microphone_available: bool,
) -> Vec<String> {
    let mut arguments = pick_args(settings, notice, port, receive_available);
    if probe {
        arguments.push("--app-probe".to_owned());
    }
    if microphone_available {
        arguments.push("--app-microphone-available".to_owned());
    }
    arguments
}

#[cfg(test)]
fn microphone_controls_available() -> bool {
    microphone::available(None)
}
#[cfg(test)]
fn microphone_arguments(arguments: Vec<String>, consent: bool) -> Result<Vec<String>, Failure> {
    microphone::take_for_choice(&mut None, arguments, consent)
}

/// Only a terminal report can return the app to its picker. A launcher exit
/// alone cannot prove the viewer's outcome or cleanup, especially on macOS.
fn session_notice(report: Option<Report>, name: &str, success: bool) -> Result<String, Failure> {
    refuse_uncertain_report(report.as_ref())?;
    match report {
        Some(Report::Failed { next, .. }) => Ok(notice_text(&format!("{name}: {next}"))),
        Some(Report::Ended) if success => Ok(notice_text(&format!(
            "The session with {name} ended; local cleanup was confirmed."
        ))),
        _ => Err(Failure::new(
            "app_session_outcome_unknown",
            "The viewer did not confirm a terminal outcome and local cleanup. Inspect the original command before starting another connection.",
            1,
        )
        .uncertain()),
    }
}

#[cfg(test)]
fn session_notice_checked(
    stdout: &CompletionCapture,
    name: &str,
    success: bool,
    receive_expected: bool,
) -> Result<String, Failure> {
    session_notice_with_transfers(stdout, name, success, false, receive_expected)
}

/// The viewer, upload sources and receive disk owner each owe terminal evidence.
/// Validate every owner before another picker or viewer can start.
fn session_notice_with_transfers(
    stdout: &CompletionCapture,
    name: &str,
    success: bool,
    upload_expected: bool,
    receive_expected: bool,
) -> Result<String, Failure> {
    let report = last_report(stdout);
    // Inspect both directions even if either fails. Their original committed
    // effects survive independently; an upload failure must not erase a known
    // download, and a receiver failure must not erase host publication proof.
    let upload = upload_completion::inspect(stdout, upload_expected, report.as_ref());
    let receive = receive_completion::inspect(stdout, receive_expected, report.as_ref());
    let upload = upload.map_err(|mut failure| {
        match &receive {
            Ok(receive) => receive.preserve(&mut failure),
            Err(receive) => failure.files_receive = receive.files_receive,
        }
        failure
    })?;
    let receive = receive.map_err(|mut failure| {
        upload.preserve(&mut failure);
        failure
    })?;
    let notice = session_notice(report, name, success).map_err(|mut failure| {
        receive.preserve(&mut failure);
        upload.preserve(&mut failure);
        failure
    })?;
    Ok(upload.with_session_notice(receive.with_session_notice(notice)))
}

fn refuse_uncertain_report(report: Option<&Report>) -> Result<(), Failure> {
    if let Some(Report::Failed {
        code,
        next,
        unknown_effect,
    }) = report
    {
        let failure = if *unknown_effect {
            Failure::new(
                "app_child_outcome_unknown",
                "The previous picker or viewer reported an uncertain outcome. Inspect the original command before starting another connection.",
                1,
            )
        } else if code.ends_with("_cleanup_incomplete") {
            Failure::new(
                "app_cleanup_unconfirmed",
                "The previous picker or viewer did not confirm cleanup. Inspect the original command before starting another connection.",
                1,
            )
        } else {
            return Ok(());
        };
        eprintln!("fr app: {}", notice_text(next));
        return Err(failure.uncertain());
    }
    Ok(())
}

fn spawn_failed() -> Failure {
    Failure::new(
        "app_child_unavailable",
        "fr app could not start its own picker or viewer command. Reinstall fr and try again.",
        1,
    )
}

/// How the picker and viewer commands start.
///
/// On macOS a process this one spawns may not bring its window in front of
/// the app the user is in (cooperative activation, macOS 14 and later): the
/// returning picker opened behind other windows. When the installed
/// launcher runs this same `fr`, each command starts through Launch Services
/// as a new instance of it instead, which macOS activates like any app the
/// user opens; `open -g` keeps a refreshed, unused picker in the background.
struct Launcher {
    exe: PathBuf,
    #[cfg(target_os = "macos")]
    bundle: Option<PathBuf>,
}

/// `open` arguments that start `bundle` as a new instance with `args`, wait
/// for it, and keep its standard output in `out`.
#[cfg(any(target_os = "macos", test))]
fn open_args(
    bundle: &Path,
    out: &Path,
    foreground: bool,
    args: &[String],
) -> Vec<std::ffi::OsString> {
    let mut list: Vec<std::ffi::OsString> = vec!["-n".into(), "-W".into()];
    if !foreground {
        list.push("-g".into());
    }
    list.extend([
        "--stdout".into(),
        out.as_os_str().to_owned(),
        "--stderr".into(),
        "/dev/null".into(),
        "-a".into(),
        bundle.as_os_str().to_owned(),
        "--args".into(),
    ]);
    list.extend(args.iter().map(Into::into));
    list
}

impl Launcher {
    fn new() -> Result<Self, Failure> {
        let exe = std::env::current_exe().map_err(|_| spawn_failed())?;
        #[cfg(target_os = "macos")]
        {
            // Only a launcher this command wrote for this same fr; any other
            // bundle of that name could run a different program.
            let bundle = home().ok().map(|home| bundle(&home)).filter(|app| {
                let script = std::fs::read_to_string(app.join("Contents/MacOS/FrankenRemote"));
                let exe = exe
                    .canonicalize()
                    .ok()
                    .and_then(|exe| exe.to_str().map(str::to_owned));
                matches!((script, exe), (Ok(script), Some(exe))
                    if script.contains(LAUNCHER_MARK) && script.contains(&shell_quote(&exe)))
            });
            Ok(Self { exe, bundle })
        }
        #[cfg(not(target_os = "macos"))]
        Ok(Self { exe })
    }

    fn run(&self, args: &[String], foreground: bool) -> Result<Finished, Failure> {
        #[cfg(target_os = "macos")]
        if let Some(bundle) = &self.bundle {
            return child::run_bundle(bundle, args, foreground).map_err(child_failed);
        }
        let _ = foreground;
        child::run(Command::new(&self.exe).args(args)).map_err(child_failed)
    }
}

fn child_failed(error: child::Error) -> Failure {
    let (code, next) = match error {
        child::Error::Start => return spawn_failed(),
        child::Error::Capture => (
            "app_child_output_incomplete",
            "The picker or viewer output could not be followed completely. Its outcome and cleanup may be unknown; inspect the original command before restarting fr app.",
        ),
        child::Error::Wait => (
            "app_child_wait_failed",
            "The picker or viewer process exit could not be confirmed. Inspect the original command before restarting fr app.",
        ),
        child::Error::Cleanup => (
            "app_child_cleanup_unconfirmed",
            "The original picker or viewer command could not be reaped. Check that it has stopped before restarting fr app.",
        ),
    };
    Failure::new(code, next, 1).uncertain()
}

fn application(
    port: u16,
    mut receiving: Option<Receiving>,
    mut microphone: Option<Microphone>,
    probe: bool,
) -> Result<String, Failure> {
    let launcher = Launcher::new()?;
    let path = settings_path(&home()?);
    let mut notice: Option<String> = None;
    let mut picker_failures = 0_u8;
    let mut idle_expiries = 0_u8;
    loop {
        let mut settings = load(&path);
        let receiving_notice = receiving::notice(receiving.as_ref(), notice.as_deref());
        // A picker reopened only because the last one expired unused stays
        // behind whatever the user is doing.
        let picked = launcher.run(
            &probed_pick_args(
                &settings,
                receiving_notice.as_deref().or(notice.as_deref()),
                port,
                receiving::available(receiving.as_ref()),
                probe,
                microphone::available(microphone.as_ref()),
            ),
            idle_expiries == 0,
        )?;
        let report = picker_report_with_microphone(
            &picked,
            port,
            receiving::available(receiving.as_ref()),
            microphone::available(microphone.as_ref()),
        );
        refuse_uncertain_report(report.as_ref())?;
        let (node, name, send_files, receive_files, microphone_controls) = match report {
            Some(Report::Selected {
                node,
                name,
                control,
                clipboard,
                choose_display,
                playback_audio,
                send_files,
                receive_files,
                microphone_controls,
                ..
            }) => {
                settings.control = control;
                settings.clipboard = clipboard;
                settings.choose_display = choose_display;
                settings.playback_audio = playback_audio;
                settings.last_machine = Some(node.clone());
                save(&path, &settings);
                (node, name, send_files, receive_files, microphone_controls)
            }
            // Closing the picker quits.
            Some(Report::Failed { code, .. }) if code == "host_choice_cancelled" => {
                return Ok(String::new());
            }
            // A choice expires after two minutes, with its machine snapshot.
            // Reopen with a fresh one, as the Windows app refreshes in place,
            // but quit once the picker has gone unused for half an hour.
            Some(Report::Failed { code, .. }) if code == "host_choice_expired" => {
                idle_expiries += 1;
                if idle_expiries >= MAX_IDLE_EXPIRIES {
                    return Ok(String::new());
                }
                continue;
            }
            other => {
                // A second failure in a row would only reopen a failing window.
                picker_failures += 1;
                let next = match other {
                    Some(Report::Failed { next, .. }) if !next.is_empty() => next,
                    _ => "The machine picker could not open.".to_owned(),
                };
                if picker_failures > 1 {
                    eprintln!("fr app: {next}");
                    return Err(Failure::new(
                        "app_picker_unavailable",
                        "The machine picker could not open. Run fr connect --choose-host --view-only --experimental-native --display only in a terminal from your desktop session to see why.",
                        1,
                    ));
                }
                notice = Some(notice_text(&next));
                continue;
            }
        };
        picker_failures = 0;
        idle_expiries = 0;
        // Consume the original local choice before validating or launching the
        // selected viewer, including an explicit decline. No next picker or
        // failed launch can recover this one-use destination.
        let receive_expected = receive_files;
        let arguments = receiving::take_for_app_choice(
            &mut receiving,
            microphone::take_for_choice(
                &mut microphone,
                session_args(&node, &settings, port, send_files),
                microphone_controls,
            )?,
            receive_files,
        )?;
        let session = launcher.run(&arguments, true)?;
        notice = Some(session_notice_with_transfers(
            &session.stdout,
            short_name(&name),
            session.status.success(),
            send_files,
            receive_expected,
        )?);
    }
}

/// `'...'` for a POSIX shell, with embedded quotes closed and escaped.
#[cfg(any(target_os = "macos", test))]
fn shell_quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// A desktop-entry `Exec` argument: double-quoted, with the reserved
/// characters escaped and `%` doubled (freedesktop Desktop Entry spec).
#[cfg(any(not(target_os = "macos"), test))]
fn desktop_quote(path: &str) -> String {
    let mut quoted = String::from("\"");
    for c in path.chars() {
        match c {
            '"' | '`' | '$' | '\\' => {
                quoted.push('\\');
                quoted.push(c);
            }
            '%' => quoted.push_str("%%"),
            _ => quoted.push(c),
        }
    }
    quoted.push('"');
    quoted
}

fn launcher_failed() -> Failure {
    Failure::new(
        "app_launcher_write_failed",
        "The launcher could not be written. Check that your home directory is writable.",
        1,
    )
}

fn executable() -> Result<String, Failure> {
    let exe = std::env::current_exe()
        .and_then(|exe| exe.canonicalize())
        .map_err(|_| spawn_failed())?;
    exe.to_str()
        .filter(|path| !path.contains('\n'))
        .map(str::to_owned)
        .ok_or(Failure::new(
            "app_path_unsupported",
            "fr is installed at a path that a launcher cannot name (it must be UTF-8 without line breaks).",
            2,
        ))
}

#[cfg(target_os = "macos")]
fn bundle(home: &Path) -> PathBuf {
    home.join("Applications/FrankenRemote.app")
}

#[cfg(target_os = "macos")]
fn info_plist() -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n<dict>\n\
<key>CFBundleName</key><string>FrankenRemote</string>\n\
<key>CFBundleDisplayName</key><string>FrankenRemote</string>\n\
<key>CFBundleIdentifier</key><string>{BUNDLE_ID}</string>\n\
<key>CFBundleExecutable</key><string>FrankenRemote</string>\n\
<key>CFBundlePackageType</key><string>APPL</string>\n\
<key>CFBundleShortVersionString</key><string>{version}</string>\n\
<key>CFBundleVersion</key><string>{version}</string>\n\
<key>LSMinimumSystemVersion</key><string>13.0</string>\n\
<key>NSHighResolutionCapable</key><true/>\n\
<key>NSMicrophoneUsageDescription</key><string>Forward your selected microphone only after you choose Allow microphone and then Talk for this session. Mute stops forwarding.</string>\n\
</dict>\n</plist>\n",
        version = env!("CARGO_PKG_VERSION")
    )
}

/// Marks the launcher script this command wrote (see [`Launcher`]).
#[cfg(any(target_os = "macos", test))]
const LAUNCHER_MARK: &str = "# Written by `fr app --install`";

/// Opened alone, the launcher runs `fr app`; `fr app` itself starts its
/// picker and viewer commands through it with their arguments. A Finder
/// process-serial-number argument from older macOS is dropped.
#[cfg(any(target_os = "macos", test))]
fn launcher_script(exe: &str) -> String {
    format!(
        "#!/bin/sh\n{LAUNCHER_MARK}: opens the FrankenRemote machine picker.\n\
case \"$1\" in -psn_*) shift ;; esac\n\
[ \"$#\" -eq 0 ] && set -- app\n\
exec {} \"$@\"\n",
        shell_quote(exe)
    )
}

#[cfg(target_os = "macos")]
fn install() -> Result<String, Failure> {
    use std::os::unix::fs::PermissionsExt;
    let exe = executable()?;
    let app = bundle(&home()?);
    let macos = app.join("Contents/MacOS");
    std::fs::create_dir_all(&macos).map_err(|_| launcher_failed())?;
    std::fs::write(app.join("Contents/Info.plist"), info_plist()).map_err(|_| launcher_failed())?;
    let script = macos.join("FrankenRemote");
    std::fs::write(&script, launcher_script(&exe)).map_err(|_| launcher_failed())?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
        .map_err(|_| launcher_failed())?;
    Ok(format!(
        "Added {} (open FrankenRemote from Spotlight, Launchpad or ~/Applications).\n",
        app.display()
    ))
}

#[cfg(target_os = "macos")]
fn uninstall() -> Result<String, Failure> {
    let app = bundle(&home()?);
    let ours = std::fs::read_to_string(app.join("Contents/Info.plist"))
        .is_ok_and(|plist| plist.contains(BUNDLE_ID));
    if !ours {
        return Ok(format!("No FrankenRemote launcher at {}.\n", app.display()));
    }
    std::fs::remove_dir_all(&app).map_err(|_| launcher_failed())?;
    Ok(format!("Removed {}.\n", app.display()))
}

#[cfg(not(target_os = "macos"))]
fn desktop_entry_path(home: &Path) -> PathBuf {
    absolute_env("XDG_DATA_HOME")
        .unwrap_or_else(|| home.join(".local/share"))
        .join("applications/frankenremote.desktop")
}

#[cfg(any(not(target_os = "macos"), test))]
fn desktop_entry(exe: &str) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nVersion=1.0\nName=FrankenRemote\n\
GenericName=Remote Desktop\nComment=Connect to a machine on your tailnet\n\
Exec={} app\nTerminal=false\nCategories=Network;RemoteAccess;\nStartupNotify=false\n\
{DESKTOP_MARKER}\n",
        desktop_quote(exe)
    )
}

#[cfg(not(target_os = "macos"))]
fn install() -> Result<String, Failure> {
    let exe = executable()?;
    let entry = desktop_entry_path(&home()?);
    if let Some(parent) = entry.parent() {
        std::fs::create_dir_all(parent).map_err(|_| launcher_failed())?;
    }
    std::fs::write(&entry, desktop_entry(&exe)).map_err(|_| launcher_failed())?;
    Ok(format!(
        "Added {} (FrankenRemote is now in your applications menu).\n",
        entry.display()
    ))
}

#[cfg(not(target_os = "macos"))]
fn uninstall() -> Result<String, Failure> {
    let entry = desktop_entry_path(&home()?);
    let ours = std::fs::read_to_string(&entry).is_ok_and(|text| text.contains(DESKTOP_MARKER));
    if !ours {
        return Ok(format!(
            "No FrankenRemote launcher at {}.\n",
            entry.display()
        ));
    }
    std::fs::remove_file(&entry).map_err(|_| launcher_failed())?;
    Ok(format!("Removed {}.\n", entry.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_app_probes_belong_only_to_this_runs_picker_commands() {
        let parse =
            |args: &[&str]| app_command(&args.iter().map(ToString::to_string).collect::<Vec<_>>());
        assert_eq!(
            parse(&["app", "--probe", "--port", "9443"]).unwrap(),
            AppCommand::Open {
                port: 9443,
                receiving: None,
                microphone: None,
                probe: true
            }
        );
        for args in [
            vec!["app", "--probe", "--probe"],
            vec!["app", "--probe", "--install"],
            vec!["app", "--uninstall", "--probe"],
        ] {
            assert_eq!(parse(&args).unwrap_err().code, "invalid_arguments");
        }
        let settings = Settings::default();
        for probe in [false, true] {
            let arguments = probed_pick_args(
                &settings,
                Some("Previous outcome"),
                9443,
                false,
                probe,
                false,
            );
            let crate::options::Command::Connect(connection) =
                crate::options::parse(&arguments).unwrap().command
            else {
                panic!("picker")
            };
            assert_eq!(connection.pick.unwrap().probe, probe);
            assert_eq!(connection.target.port, 9443);
            assert!(
                !session_args("n-original", &settings, 9443, false)
                    .iter()
                    .any(|arg| arg.contains("probe"))
            );
        }
        assert!(!settings.to_json().contains("probe"));
    }

    #[test]
    fn settings_round_trip_and_keep_defaults_for_bad_fields() {
        let settings = Settings {
            control: false,
            clipboard: true,
            choose_display: true,
            playback_audio: true,
            last_machine: Some("nABC123CNTRL".to_owned()),
        };
        assert_eq!(Settings::from_json(settings.to_json().as_bytes()), settings);
        assert_eq!(Settings::from_json(b"not json"), Settings::default());
        assert_eq!(
            Settings::from_json(br#"{"version":2,"control":false}"#),
            Settings::default()
        );
        let partial = Settings::from_json(
            br#"{"version":1,"control":"yes","clipboard":false,"playback_audio":"yes","last_machine":42}"#,
        );
        assert!(partial.control && !partial.clipboard && partial.last_machine.is_none());
        assert!(!partial.playback_audio);
        // Existing version-one preferences never implicitly enable audio.
        assert!(!Settings::from_json(br#"{"version":1,"control":true}"#).playback_audio);
        let long = format!(
            r#"{{"version":1,"last_machine":"{}"}}"#,
            "n".repeat(MAX_ID_BYTES + 1)
        );
        assert_eq!(Settings::from_json(long.as_bytes()).last_machine, None);
    }

    #[test]
    fn saved_choices_load_back_and_oversized_or_missing_files_use_defaults() {
        let dir = std::env::temp_dir().join(format!("fr-app-settings-{}", std::process::id()));
        let path = dir.join("nested/settings.json");
        let settings = Settings {
            control: false,
            last_machine: Some("nMAC".to_owned()),
            ..Settings::default()
        };
        save(&path, &settings);
        assert_eq!(load(&path), settings);
        std::fs::write(&path, vec![b' '; 5000]).unwrap();
        assert_eq!(load(&path), Settings::default());
        assert_eq!(load(&dir.join("missing.json")), Settings::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_port_is_strict_and_launch_scoped_before_any_side_effect() {
        let parse =
            |args: &[&str]| app_command(&args.iter().map(ToString::to_string).collect::<Vec<_>>());
        assert_eq!(
            parse(&["app"]).unwrap(),
            AppCommand::Open {
                port: DEFAULT_SERVICE_PORT,
                receiving: None,
                microphone: None,
                probe: false,
            }
        );
        for port in [1, 9443, u16::MAX] {
            let text = port.to_string();
            assert_eq!(
                parse(&["app", "--port", &text, "--json"]).unwrap(),
                AppCommand::Open {
                    port,
                    receiving: None,
                    microphone: None,
                    probe: false,
                }
            );
            assert_eq!(
                parse(&["app", "--json", "--port", &text]).unwrap(),
                AppCommand::Open {
                    port,
                    receiving: None,
                    microphone: None,
                    probe: false,
                }
            );
        }
        for args in [
            vec!["app", "--port"],
            vec!["app", "--port", "--json", "9443"],
            vec!["app", "--port", "9443", "--port", "9443"],
            vec!["app", "--port", "9443", "--install"],
            vec!["app", "--uninstall", "--port", "9443"],
            vec!["app", "--json", "--json"],
            vec!["app", "--control"],
            vec!["connect", "--port", "9443"],
        ] {
            assert_eq!(parse(&args).unwrap_err().code, "invalid_arguments");
        }
        for port in [
            "",
            "0",
            "65536",
            "-1",
            "+1",
            " 9443",
            "9443 ",
            "94.43",
            "0x24e3",
            "９４４３",
        ] {
            assert_eq!(
                parse(&["app", "--port", port]).unwrap_err().code,
                "invalid_arguments"
            );
        }
        assert_eq!(parse(&["app", "--help"]).unwrap(), AppCommand::Help);
        assert_eq!(
            parse(&["app", "--install", "--json"]).unwrap(),
            AppCommand::Install
        );
        assert_eq!(
            parse(&["app", "--uninstall"]).unwrap(),
            AppCommand::Uninstall
        );
        // Saved preferences and a later unqualified launch do not inherit it.
        assert!(!Settings::default().to_json().contains("port"));
        assert_eq!(
            parse(&["app"]).unwrap(),
            AppCommand::Open {
                port: DEFAULT_SERVICE_PORT,
                receiving: None,
                microphone: None,
                probe: false,
            }
        );
    }

    #[test]
    fn every_picker_and_viewer_command_preserves_the_original_port() {
        let port = 9443;
        let mut settings = Settings::default();
        for control in [false, true] {
            settings.control = control;
            for choose_display in [false, true] {
                settings.choose_display = choose_display;
                for playback_audio in [false, true] {
                    settings.playback_audio = playback_audio;
                    for node in ["nFIRST", "nNEXT"] {
                        let picker = pick_args(
                            &settings,
                            Some("The last host did not answer."),
                            port,
                            false,
                        );
                        let viewer = session_args(node, &settings, port, true);
                        for (args, picking) in [(&picker, true), (&viewer, false)] {
                            let parsed = crate::options::parse(args).unwrap();
                            let crate::options::Command::Connect(connection) = parsed.command
                            else {
                                panic!("real connect command")
                            };
                            assert_eq!(connection.target.port, port);
                            assert_eq!(connection.control, control);
                            assert_eq!(connection.upload.file_drop, !picking && control);
                            assert_eq!(
                                connection.display,
                                if choose_display {
                                    crate::options::DisplayChoice::Choose
                                } else {
                                    crate::options::DisplayChoice::Only
                                }
                            );
                            assert!(connection.microphone.is_none());
                            assert_eq!(args.iter().filter(|arg| *arg == "--port").count(), 1);
                            assert_eq!(
                                args.iter().filter(|arg| *arg == "--audio").count(),
                                usize::from(playback_audio)
                            );
                            if picking {
                                let choice = connection.pick.unwrap();
                                assert_eq!(choice.playback_audio, playback_audio);
                                assert!(connection.audio.is_none());
                            } else {
                                assert!(connection.pick.is_none());
                                assert_eq!(connection.audio.is_some(), playback_audio);
                            }
                        }
                        let notice = picker
                            .windows(2)
                            .find(|pair| pair[0] == "--app-notice")
                            .unwrap();
                        assert_eq!(
                            notice[1],
                            "Service port 9443. The last host did not answer."
                        );
                        // Launch Services receives the same endpoint, audio
                        // choice and original file-drop opt-in.
                        let open = open_args(Path::new("/A.app"), Path::new("/out"), true, &viewer);
                        let boundary = open.iter().position(|arg| arg == "--args").unwrap();
                        assert_eq!(
                            open[boundary + 1..],
                            viewer
                                .iter()
                                .map(std::ffi::OsString::from)
                                .collect::<Vec<_>>()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn picker_cannot_retarget_the_original_port_or_override_failed_exit() {
        use std::os::unix::process::ExitStatusExt;
        let mut selected = json!({
            "schema_version": 1, "event": "host_selected", "node_id": "nHOST",
            "certificate_name": "host.tail.ts.net", "port": 9443,
            "control": false, "clipboard": false, "choose_display": false,
            "playback_audio": false, "send_files": false, "receive_files": false, "microphone_controls": false,
            "selection_only": true, "fresh_lookup_required": true,
            "remote_session_started": false, "input_requested": false,
        });
        let result = |value: &Value, exit| Finished {
            stdout: fr_client::completion::capture_completion_with_limit(
                value.to_string().as_bytes(),
                fr_client::completion::CompletionLimit::NativeCommand,
            ),
            status: std::process::ExitStatus::from_raw(exit),
        };
        for playback_audio in [false, true] {
            selected["playback_audio"] = json!(playback_audio);
            assert!(matches!(
                picker_report(&result(&selected, 0), 9443, false),
                Some(Report::Selected { port: 9443, playback_audio: actual, .. })
                    if actual == playback_audio
            ));
            assert_eq!(picker_report(&result(&selected, 0), 8443, false), None);
            assert_eq!(picker_report(&result(&selected, 256), 9443, false), None);
            for value in [
                Value::Null,
                json!(0),
                json!(65536),
                json!(-1),
                json!("9443"),
            ] {
                let mut changed = selected.clone();
                changed["port"] = value;
                assert_eq!(picker_report(&result(&changed, 0), 9443, false), None);
            }
            let mut changed = selected.clone();
            changed.as_object_mut().unwrap().remove("port");
            assert_eq!(picker_report(&result(&changed, 0), 9443, false), None);
        }
        let cancelled = json!({"schema_version":1,"outcome":"cancelled","error":{"code":"host_choice_cancelled","next_action":"Choose again explicitly."}});
        assert!(matches!(
            picker_report(&result(&cancelled, 256), 9443, false),
            Some(Report::Failed {
                unknown_effect: false,
                ..
            })
        ));
    }

    #[test]
    fn picker_and_session_commands_follow_the_choices() {
        let mut settings = Settings::default();
        let pick = pick_args(
            &settings,
            Some("mac: The host did not answer."),
            DEFAULT_SERVICE_PORT,
            false,
        );
        assert_eq!(
            pick,
            [
                "connect",
                "--choose-host",
                "--pick-only",
                "--json",
                "--port",
                "8443",
                "--experimental-native",
                "--display",
                "only",
                "--control",
                "--clipboard",
                "--app-notice",
                "mac: The host did not answer.",
            ]
        );
        assert!(crate::options::parse(&pick).is_ok());
        assert_eq!(
            session_args("nMAC", &settings, DEFAULT_SERVICE_PORT, false),
            [
                "connect",
                "nMAC",
                "--json",
                "--port",
                "8443",
                "--experimental-native",
                "--display",
                "only",
                "--control"
            ]
        );
        settings.clipboard = false;
        settings.choose_display = true;
        assert!(
            session_args("nMAC", &settings, DEFAULT_SERVICE_PORT, false)
                .ends_with(&["--control".into(), "--no-clipboard".into()])
        );
        assert!(
            session_args("nMAC", &settings, DEFAULT_SERVICE_PORT, false)
                .contains(&"choose".to_owned())
        );
        // View-only remembers the clipboard choice for the picker only.
        settings.control = false;
        settings.clipboard = true;
        settings.choose_display = false;
        let pick = pick_args(&settings, None, DEFAULT_SERVICE_PORT, false);
        assert!(pick.contains(&"--view-only".into()) && pick.contains(&"--clipboard".into()));
        assert!(crate::options::parse(&pick).is_ok());
        assert_eq!(
            session_args("nMAC", &settings, DEFAULT_SERVICE_PORT, false),
            [
                "connect",
                "nMAC",
                "--json",
                "--port",
                "8443",
                "--experimental-native",
                "--display",
                "only",
                "--view-only"
            ]
        );
    }

    #[test]
    fn audio_is_opt_in_for_both_roles_and_never_enables_the_microphone() {
        for control in [false, true] {
            let mut settings = Settings {
                control,
                ..Settings::default()
            };
            for audio in [false, true] {
                settings.playback_audio = audio;
                let picker = pick_args(&settings, None, DEFAULT_SERVICE_PORT, false);
                let session = session_args("nMAC", &settings, DEFAULT_SERVICE_PORT, false);
                for args in [&picker, &session] {
                    assert_eq!(args.iter().any(|arg| arg == "--audio"), audio);
                    assert!(!args.iter().any(|arg| arg.starts_with("--microphone")));
                }
                let crate::options::Command::Connect(picker) =
                    crate::options::parse(&picker).unwrap().command
                else {
                    panic!("expected picker connection");
                };
                assert_eq!(picker.pick.unwrap().playback_audio, audio);
                assert!(picker.audio.is_none() && picker.microphone.is_none());
                let crate::options::Command::Connect(session) =
                    crate::options::parse(&session).unwrap().command
                else {
                    panic!("expected viewer connection");
                };
                assert_eq!(session.audio.is_some(), audio);
                assert_eq!(session.control, control);
                assert!(session.microphone.is_none());
            }
        }
    }

    #[test]
    fn microphone_controls_require_fresh_consent_and_never_become_preferences() {
        let available = microphone_controls_available();
        let mut settings = Settings::from_json(
            br#"{"version":1,"microphone":true,"microphone_controls":true,"microphone_source":"old-device"}"#,
        );
        assert!(!settings.to_json().contains("microphone"));
        for control in [false, true] {
            settings.control = control;
            for audio in [false, true] {
                settings.playback_audio = audio;
                for consent in [false, true, false] {
                    let args = session_args("nHOST", &settings, 9443, false);
                    let requested = microphone_arguments(args, consent);
                    if consent && (!available || !control) {
                        let failure = requested.unwrap_err();
                        assert_eq!(
                            failure.code,
                            if available {
                                "microphone_requires_control"
                            } else {
                                "app_microphone_unavailable"
                            }
                        );
                        continue;
                    }
                    let crate::options::Command::Connect(viewer) =
                        crate::options::parse(&requested.unwrap()).unwrap().command
                    else {
                        panic!("ordinary viewer");
                    };
                    assert_eq!(viewer.audio.is_some(), audio);
                    assert_eq!(viewer.microphone.is_some(), consent);
                    if let Some(microphone) = viewer.microphone {
                        assert_eq!(microphone.source, "default");
                        assert!(microphone.server.is_none());
                    }
                    let crate::options::Command::Connect(picker) =
                        crate::options::parse(&pick_args(&settings, None, 9443, false))
                            .unwrap()
                            .command
                    else {
                        panic!("fresh selection-only picker");
                    };
                    assert!(picker.microphone.is_none());
                    assert!(
                        !crate::host_choice::Choices::initial(&picker)
                            .unwrap()
                            .microphone_controls
                    );
                }
            }
        }
    }

    #[test]
    fn microphone_reports_require_boolean_consent_control_local_support_and_child_success() {
        use std::os::unix::process::ExitStatusExt;
        let original = json!({
            "schema_version": 1, "event": "host_selected", "node_id": "nHOST",
            "certificate_name": "host.fixture.ts.net", "port": 9443,
            "selection_only": true, "fresh_lookup_required": true,
            "remote_session_started": false, "input_requested": false,
            "control": true, "clipboard": false, "choose_display": false,
            "playback_audio": false, "send_files": false, "receive_files": false,
            "microphone_controls": false,
        });
        for control in [false, true] {
            for consent in [false, true] {
                for success in [false, true] {
                    let mut value = original.clone();
                    value["control"] = json!(control);
                    value["microphone_controls"] = json!(consent);
                    let finished = Finished {
                        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
                        stdout: fr_client::completion::capture_completion_with_limit(
                            value.to_string().as_bytes(),
                            fr_client::completion::CompletionLimit::NativeCommand,
                        ),
                    };
                    assert_eq!(
                        picker_report(&finished, 9443, false).is_some(),
                        success && (!consent || (control && microphone_controls_available()))
                    );
                }
            }
        }
        for invalid in [Value::Null, json!("true"), json!(1), json!({})] {
            let mut value = original.clone();
            value["microphone_controls"] = invalid;
            assert_eq!(last_report(value.to_string().as_bytes()), None);
        }
        let mut missing = original;
        missing
            .as_object_mut()
            .unwrap()
            .remove("microphone_controls");
        assert_eq!(last_report(missing.to_string().as_bytes()), None);
    }

    #[test]
    fn file_sending_is_one_use_control_only_and_never_a_saved_picker_preference() {
        let mut settings = Settings::from_json(
            br#"{"version":1,"send_files":true,"receive_files":false,"microphone_controls":false,"file_drop":true}"#,
        );
        let saved: Value = serde_json::from_str(&settings.to_json()).unwrap();
        assert!(saved.get("send_files").is_none() && saved.get("file_drop").is_none());
        for control in [false, true] {
            settings.control = control;
            for send_files in [false, true] {
                let args = session_args("nMAC", &settings, 9443, send_files);
                assert_eq!(
                    args.iter().any(|arg| arg == "--file-drop"),
                    control && send_files
                );
                let crate::options::Command::Connect(session) =
                    crate::options::parse(&args).unwrap().command
                else {
                    panic!("expected viewer connection");
                };
                assert_eq!(session.upload.file_drop, control && send_files);
                assert_eq!(session.target.port, 9443);
                assert_eq!(session.upload.paths, [] as [std::path::PathBuf; 0]);
                assert!(!session.upload.choose_files && !session.upload.file_commands);
                assert!(session.receive.is_none());
                // A new picker never inherits the previous session's consent.
                let args = pick_args(&settings, None, 9443, false);
                assert!(!args.iter().any(|arg| arg == "--file-drop"));
                let crate::options::Command::Connect(picker) =
                    crate::options::parse(&args).unwrap().command
                else {
                    panic!("expected picker connection");
                };
                assert!(!picker.uploads() && picker.receive.is_none());
                assert_eq!(picker.target.port, 9443);
            }
        }
    }

    #[test]
    fn child_reports_become_selections_exits_and_notices() {
        let selected = br#"{"schema_version":1,"event":"host_selected","node_id":"nMAC","certificate_name":"mac.tail.ts.net","port":8443,"control":true,"clipboard":false,"choose_display":false,"playback_audio":true,"send_files":true,"receive_files":false,"microphone_controls":false,"selection_only":true,"fresh_lookup_required":true,"remote_session_started":false,"input_requested":false}
"#;
        assert_eq!(
            last_report(selected),
            Some(Report::Selected {
                node: "nMAC".into(),
                name: "mac.tail.ts.net".into(),
                port: DEFAULT_SERVICE_PORT,
                control: true,
                clipboard: false,
                choose_display: false,
                playback_audio: true,
                send_files: true,
                receive_files: false,
                microphone_controls: false,
            })
        );
        let mut malformed: Value = serde_json::from_slice(selected).unwrap();
        malformed["playback_audio"] = json!("yes");
        assert_eq!(last_report(malformed.to_string().as_bytes()), None);
        malformed.as_object_mut().unwrap().remove("playback_audio");
        assert_eq!(last_report(malformed.to_string().as_bytes()), None);
        let mut malformed: Value = serde_json::from_slice(selected).unwrap();
        malformed["send_files"] = json!("yes");
        assert_eq!(last_report(malformed.to_string().as_bytes()), None);
        malformed.as_object_mut().unwrap().remove("send_files");
        assert_eq!(last_report(malformed.to_string().as_bytes()), None);
        malformed["send_files"] = json!(true);
        malformed["control"] = json!(false);
        assert_eq!(last_report(malformed.to_string().as_bytes()), None);
        malformed["send_files"] = json!(false);
        assert!(matches!(
            last_report(malformed.to_string().as_bytes()),
            Some(Report::Selected {
                control: false,
                send_files: false,
                ..
            })
        ));
        // A selection without choices is not a picker answer.
        assert_eq!(
            last_report(br#"{"event":"host_selected","node_id":"nMAC","certificate_name":"m"}"#),
            None
        );
        let failed = b"progress\n{\"schema_version\":1,\"outcome\":\"refused\",\"error\":{\"code\":\"host_not_answering\",\"next_action\":\"Check that frd run is running there.\"}}\n";
        let report = last_report(failed);
        assert_eq!(
            report,
            Some(Report::Failed {
                code: "host_not_answering".into(),
                next: "Check that frd run is running there.".into(),
                unknown_effect: false,
            })
        );
        assert_eq!(
            session_notice(report, "mac", false).unwrap(),
            "mac: Check that frd run is running there."
        );
        assert_eq!(
            session_notice(Some(Report::Ended), "mac", true).unwrap(),
            "The session with mac ended; local cleanup was confirmed."
        );
        assert!(
            session_notice(None, "mac", false)
                .unwrap_err()
                .unknown_effect
        );
        assert_eq!(last_report(b"no json at all\n"), None);
        assert_eq!(short_name("mac-mini-max.tail1f21e.ts.net"), "mac-mini-max");
    }

    fn last_report(bytes: &[u8]) -> Option<Report> {
        super::last_report(&fr_client::completion::capture_completion_with_limit(
            bytes,
            fr_client::completion::CompletionLimit::NativeCommand,
        ))
    }

    #[test]
    fn only_native_terminal_cleanup_reports_support_a_normal_notice() {
        for document in [
            r#"{"schema_version":1,"outcome":"stopped","role":"observe","cleanup_confirmed":true}"#,
            r#"{"schema_version":1,"outcome":"stopped","role":"control","cleanup_confirmed":true}"#,
            r#"{"outcome":"success","platform":"macos","ended":"window_closed","local_cleanup_confirmed":true}"#,
        ] {
            assert_eq!(last_report(document.as_bytes()), Some(Report::Ended));
            assert!(
                session_notice(last_report(document.as_bytes()), "mac", true)
                    .unwrap()
                    .contains("local cleanup was confirmed")
            );
            assert!(
                session_notice(last_report(document.as_bytes()), "mac", false)
                    .unwrap_err()
                    .unknown_effect
            );
        }
        for document in [
            "{}",
            "{broken",
            "null",
            "[]",
            "",
            r#"{"outcome":"success"}"#,
            r#"{"schema_version":1,"outcome":"success","snapshot_only":true}"#,
            r#"{"schema_version":2,"outcome":"stopped","role":"observe","cleanup_confirmed":true}"#,
            r#"{"schema_version":1,"outcome":"stopped","role":"observe","cleanup_confirmed":false}"#,
            r#"{"outcome":"success","platform":"macos","ended":"window_closed","local_cleanup_confirmed":false}"#,
            r#"{"outcome":"success","platform":"macos","local_cleanup_confirmed":true}"#,
            r#"{"schema_version":2,"outcome":"success","platform":"macos","ended":"window_closed","local_cleanup_confirmed":true}"#,
        ] {
            assert_eq!(last_report(document.as_bytes()), None, "{document}");
            assert!(
                session_notice(last_report(document.as_bytes()), "mac", true)
                    .unwrap_err()
                    .unknown_effect
            );
        }
    }

    #[test]
    fn terminal_errors_cannot_be_replaced_by_earlier_completion() {
        let end =
            r#"{"schema_version":1,"outcome":"stopped","role":"observe","cleanup_confirmed":true}"#;
        for tail in ["{broken", "viewer failed", "{}"] {
            assert_eq!(last_report(format!("{end}\n{tail}\n").as_bytes()), None);
        }
        let error = r#"{"schema_version":1,"outcome":"unknown_external_effect","error":{"code":"disconnect_outcome_unknown","next_action":"Inspect outstanding effects before continuing."}}"#;
        assert!(
            session_notice(
                last_report(format!("{end}\n{error}\n").as_bytes()),
                "mac",
                true
            )
            .unwrap_err()
            .unknown_effect
        );
        for code in [
            "host_picker_cleanup_incomplete",
            "native_cleanup_incomplete",
            "microphone_cleanup_incomplete",
            "host_cleanup_incomplete",
        ] {
            let report = Report::Failed {
                code: code.into(),
                next: "Inspect the original worker.".into(),
                unknown_effect: false,
            };
            assert!(refuse_uncertain_report(Some(&report)).is_err());
        }
    }

    #[test]
    fn incomplete_child_outcomes_remain_unknown_in_the_json_envelope() {
        for error in [
            child::Error::Capture,
            child::Error::Wait,
            child::Error::Cleanup,
        ] {
            let document = crate::output::failure(child_failed(error), true);
            let value: Value = serde_json::from_str(&document).unwrap();
            assert_eq!(value["outcome"], "unknown_external_effect");
        }
        assert!(!child_failed(child::Error::Start).unknown_effect);
        let report = Report::Failed {
            code: "native_cleanup_incomplete".into(),
            next: "Inspect the original worker.".into(),
            unknown_effect: false,
        };
        assert!(
            refuse_uncertain_report(Some(&report))
                .unwrap_err()
                .unknown_effect
        );
    }

    #[test]
    fn uncertain_reports_stop_before_picker_retry_or_cancel_handling() {
        for code in [
            "disconnect_outcome_unknown",
            "host_choice_cancelled",
            "host_choice_expired",
        ] {
            let document = json!({
                "schema_version": 1,
                "outcome": "unknown_external_effect",
                "error": {"code": code, "next_action": "Inspect the original command."},
            })
            .to_string();
            let report = last_report(document.as_bytes());
            assert!(matches!(
                report,
                Some(Report::Failed {
                    unknown_effect: true,
                    ..
                })
            ));
            let picker_error = refuse_uncertain_report(report.as_ref()).unwrap_err();
            assert!(picker_error.unknown_effect);
            for success in [true, false] {
                let viewer_error =
                    session_notice(last_report(document.as_bytes()), "mac", success).unwrap_err();
                assert_eq!(viewer_error, picker_error);
                let output = crate::output::failure(viewer_error, true);
                let value: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(value["outcome"], "unknown_external_effect");
                assert_eq!(value["error"]["code"], "app_child_outcome_unknown");
            }
        }
    }

    #[test]
    fn a_nonterminal_viewer_report_cannot_start_another_session() {
        let selected = json!({
            "schema_version": 1,
            "event": "host_selected",
            "node_id": "nMAC",
            "certificate_name": "mac.tail.ts.net",
            "port": DEFAULT_SERVICE_PORT,
            "control": false,
            "clipboard": false,
            "choose_display": false,
            "playback_audio": false,
            "send_files": false, "receive_files": false, "microphone_controls": false,
            "selection_only": true,
            "fresh_lookup_required": true,
            "remote_session_started": false,
            "input_requested": false,
        })
        .to_string();
        assert!(matches!(
            last_report(selected.as_bytes()),
            Some(Report::Selected { .. })
        ));
        for document in [
            selected.as_bytes(),
            b"",
            b"{}",
            b"{broken",
            b"{\"outcome\":\"success\"}",
            b"\xff\n",
        ] {
            for success in [true, false] {
                let failure = session_notice(last_report(document), "mac", success).unwrap_err();
                let output = crate::output::failure(failure, true);
                let value: Value = serde_json::from_str(&output).unwrap();
                assert_eq!(value["outcome"], "unknown_external_effect");
                assert_eq!(value["error"]["code"], "app_session_outcome_unknown");
            }
        }
    }

    #[test]
    fn known_terminal_refusals_remain_retryable() {
        for (outcome, code) in [
            ("refused", "host_not_answering"),
            ("cancelled", "host_choice_cancelled"),
            ("refused", "host_choice_expired"),
            ("revoked", "host_control_revoked"),
        ] {
            let document = json!({
                "schema_version": 1,
                "outcome": outcome,
                "error": {"code": code, "next_action": "Check the original command."},
            })
            .to_string();
            assert!(refuse_uncertain_report(last_report(document.as_bytes()).as_ref()).is_ok());
            for success in [true, false] {
                assert_eq!(
                    session_notice(last_report(document.as_bytes()), "mac", success).unwrap(),
                    "mac: Check the original command."
                );
            }
        }
        // An error-shaped object without a known terminal outcome is no
        // stronger evidence than an arbitrary JSON document.
        for outcome in [Value::Null, json!("success"), json!("other")] {
            let document = json!({
                "schema_version": 1,
                "outcome": outcome,
                "error": {"code": "host_not_answering", "next_action": "Inspect the command."},
            })
            .to_string();
            assert_eq!(last_report(document.as_bytes()), None);
            assert!(
                session_notice(last_report(document.as_bytes()), "mac", true)
                    .unwrap_err()
                    .unknown_effect
            );
        }
    }

    #[test]
    fn notices_are_bounded_printable_ascii() {
        assert_eq!(notice_text("a\u{e9}\tb\n  c"), "a b c");
        let long = notice_text(&"x".repeat(1000));
        assert_eq!(long.len(), MAX_APP_NOTICE);
        assert!(long.ends_with("..."));
        assert_eq!(notice_text(""), "The last attempt ended.");
    }

    #[test]
    fn launchers_quote_the_executable_path() {
        assert_eq!(shell_quote("/a b/it's/fr"), r"'/a b/it'\''s/fr'");
        let script = launcher_script("/Users/me/Library/Application Support/FrankenRemote/bin/fr");
        assert!(script.starts_with("#!/bin/sh\n# Written by `fr app --install`"));
        assert!(script.contains("[ \"$#\" -eq 0 ] && set -- app\n"));
        assert!(script.ends_with(
            "exec '/Users/me/Library/Application Support/FrankenRemote/bin/fr' \"$@\"\n"
        ));
        let args = ["connect".to_owned(), "--choose-host".to_owned()];
        let open = open_args(
            Path::new("/Users/me/Applications/FrankenRemote.app"),
            Path::new("/tmp/out"),
            true,
            &args,
        );
        assert_eq!(
            open,
            [
                "-n",
                "-W",
                "--stdout",
                "/tmp/out",
                "--stderr",
                "/dev/null",
                "-a",
                "/Users/me/Applications/FrankenRemote.app",
                "--args",
                "connect",
                "--choose-host"
            ]
            .map(std::ffi::OsString::from)
        );
        let background = open_args(Path::new("/A.app"), Path::new("/o"), false, &args);
        assert_eq!(background[2], "-g");
        assert_eq!(desktop_quote(r#"/x/"$`\%/fr"#), r#""/x/\"\$\`\\%%/fr""#);
        let entry = desktop_entry("/home/me/.local/bin/fr");
        assert!(entry.contains("Exec=\"/home/me/.local/bin/fr\" app\n"));
        assert!(entry.contains(DESKTOP_MARKER) && entry.contains("Terminal=false"));
    }
}
