//! `fr app`: the desktop application on macOS and Linux, the counterpart of
//! the Windows `FrankenRemote.exe`. It loops over two child commands of this
//! same executable, so every connection takes the ordinary tested path:
//!
//! 1. `fr connect --choose-host --pick-only --json ...` shows the native
//!    machine picker with the connection choices (view only or view and
//!    control, clipboard text, which display) and the previous outcome, then
//!    reports the chosen identity and choices. It opens no connection.
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
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

#[path = "app/child.rs"]
mod child;
use child::Finished;

const USAGE: &str = "fr app [--install | --uninstall]\n\n\
Without options, opens the machine picker: choose a machine on your tailnet,\n\
View only or View and control, Share clipboard text and Ask which display to\n\
show, then Connect. Closing the viewer returns to the picker with the outcome;\n\
Cancel or Escape in the picker quits. Choices are remembered. An unused\n\
picker reopens with a fresh machine list every two minutes; after half an\n\
hour unused, fr app quits.\n\n\
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
    let rest: Vec<&str> = args
        .iter()
        .skip(1)
        .map(String::as_str)
        .filter(|arg| *arg != "--json")
        .collect();
    match rest.as_slice() {
        [] => application(),
        ["--help" | "-h"] => Ok(USAGE.to_owned()),
        ["--install"] => install(),
        ["--uninstall"] => uninstall(),
        _ => Err(Failure::new(
            "invalid_arguments",
            "Use fr app, fr app --install or fr app --uninstall.",
            2,
        )),
    }
}

/// The remembered choices; the same document as the Windows application's.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "three independent check boxes"
)]
struct Settings {
    control: bool,
    clipboard: bool,
    choose_display: bool,
    last_machine: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            control: true,
            clipboard: true,
            choose_display: false,
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
        control: bool,
        clipboard: bool,
        choose_display: bool,
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
        let (Some(node), Some(name), Some(control), Some(clipboard), Some(choose_display)) = (
            text("node_id").filter(|id| valid_id(id)),
            text("certificate_name"),
            flag("control"),
            flag("clipboard"),
            flag("choose_display"),
        ) else {
            return None;
        };
        return Some(Report::Selected {
            node,
            name,
            control,
            clipboard,
            choose_display,
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

fn picker_report(picked: &Finished) -> Option<Report> {
    match last_report(&picked.stdout) {
        Some(Report::Selected { .. }) if !picked.status.success() => None,
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

fn pick_args(settings: &Settings, notice: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = [
        "connect",
        "--choose-host",
        "--pick-only",
        "--json",
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
    if let Some(notice) = notice {
        args.push("--app-notice".to_owned());
        args.push(notice.to_owned());
    }
    args
}

fn session_args(node: &str, settings: &Settings) -> Vec<String> {
    let mut args: Vec<String> = [
        "connect",
        node,
        "--json",
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
    args
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

fn application() -> Result<String, Failure> {
    let launcher = Launcher::new()?;
    let path = settings_path(&home()?);
    let mut notice: Option<String> = None;
    let mut picker_failures = 0_u8;
    let mut idle_expiries = 0_u8;
    loop {
        let mut settings = load(&path);
        // A picker reopened only because the last one expired unused stays
        // behind whatever the user is doing.
        let picked = launcher.run(&pick_args(&settings, notice.as_deref()), idle_expiries == 0)?;
        let report = picker_report(&picked);
        refuse_uncertain_report(report.as_ref())?;
        let (node, name) = match report {
            Some(Report::Selected {
                node,
                name,
                control,
                clipboard,
                choose_display,
            }) => {
                settings.control = control;
                settings.clipboard = clipboard;
                settings.choose_display = choose_display;
                settings.last_machine = Some(node.clone());
                save(&path, &settings);
                (node, name)
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
        let session = launcher.run(&session_args(&node, &settings), true)?;
        let report = last_report(&session.stdout);
        notice = Some(session_notice(
            report,
            short_name(&name),
            session.status.success(),
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
    fn settings_round_trip_and_keep_defaults_for_bad_fields() {
        let settings = Settings {
            control: false,
            clipboard: true,
            choose_display: true,
            last_machine: Some("nABC123CNTRL".to_owned()),
        };
        assert_eq!(Settings::from_json(settings.to_json().as_bytes()), settings);
        assert_eq!(Settings::from_json(b"not json"), Settings::default());
        assert_eq!(
            Settings::from_json(br#"{"version":2,"control":false}"#),
            Settings::default()
        );
        let partial = Settings::from_json(
            br#"{"version":1,"control":"yes","clipboard":false,"last_machine":42}"#,
        );
        assert!(partial.control && !partial.clipboard && partial.last_machine.is_none());
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
    fn picker_and_session_commands_follow_the_choices() {
        let mut settings = Settings::default();
        let pick = pick_args(&settings, Some("mac: The host did not answer."));
        assert_eq!(
            pick,
            [
                "connect",
                "--choose-host",
                "--pick-only",
                "--json",
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
            session_args("nMAC", &settings),
            [
                "connect",
                "nMAC",
                "--json",
                "--experimental-native",
                "--display",
                "only",
                "--control"
            ]
        );
        settings.clipboard = false;
        settings.choose_display = true;
        assert!(
            session_args("nMAC", &settings)
                .ends_with(&["--control".into(), "--no-clipboard".into()])
        );
        assert!(session_args("nMAC", &settings).contains(&"choose".to_owned()));
        // View-only remembers the clipboard choice for the picker only.
        settings.control = false;
        settings.clipboard = true;
        settings.choose_display = false;
        let pick = pick_args(&settings, None);
        assert!(pick.contains(&"--view-only".into()) && pick.contains(&"--clipboard".into()));
        assert!(crate::options::parse(&pick).is_ok());
        assert_eq!(
            session_args("nMAC", &settings),
            [
                "connect",
                "nMAC",
                "--json",
                "--experimental-native",
                "--display",
                "only",
                "--view-only"
            ]
        );
    }

    #[test]
    fn child_reports_become_selections_exits_and_notices() {
        let selected = br#"{"schema_version":1,"event":"host_selected","node_id":"nMAC","certificate_name":"mac.tail.ts.net","port":8443,"control":true,"clipboard":false,"choose_display":false,"selection_only":true,"fresh_lookup_required":true,"remote_session_started":false,"input_requested":false}
"#;
        assert_eq!(
            last_report(selected),
            Some(Report::Selected {
                node: "nMAC".into(),
                name: "mac.tail.ts.net".into(),
                control: true,
                clipboard: false,
                choose_display: false,
            })
        );
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
            "control": false,
            "clipboard": false,
            "choose_display": false,
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
