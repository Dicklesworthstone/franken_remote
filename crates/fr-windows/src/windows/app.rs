//! The `FrankenRemote` desktop application: the machine picker, one viewer
//! session at a time, and back to the picker with a plain-language outcome.
//!
//! Each session runs in a child process of this same executable
//! (`--session view <stable-id> ...`), which is exactly the command-line
//! viewer: a fresh installed-Tailscale lookup resolves the chosen stable ID
//! before dialing, and every session's native state ends with its process.
//! The child's content-free completion document becomes the picker notice.
//! Unconfirmed original completion or cleanup stops this app invocation.
use super::hosts::{self, Chosen, Error, Mode, Notice, PickError, Picked, Request};
use super::install;
use crate::{
    machines, options::AppCommand, outcome::Continuation, session_output, settings::Settings,
};
use fr_tailnet::DEFAULT_SERVICE_PORT;
use std::{
    num::NonZeroU16,
    os::windows::process::CommandExt,
    path::Path,
    process::{Command, ExitCode, Stdio},
};
use windows::{
    Win32::UI::{
        Controls::{
            ICC_LISTVIEW_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX, InitCommonControlsEx,
        },
        HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext},
        WindowsAndMessaging::{
            AllowSetForegroundWindow, IDOK, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONINFORMATION,
            MB_ICONQUESTION, MB_OK, MB_OKCANCEL, MB_SETFOREGROUND, MESSAGEBOX_STYLE, MessageBoxW,
        },
    },
    core::HSTRING,
};

/// `ASFW_ANY`: let the next window to open take the foreground.
const ASFW_ANY: u32 = u32::MAX;
const USAGE: &str = "Usage: FrankenRemote [--port N] [--quiet]\n       FrankenRemote [--install | --uninstall] [--quiet]\n\nChoose a machine from your tailnet. The port must be 1-65535 and applies only to this app launch. The command-line viewer is fr-windows.exe.";

/// Open the packaged GUI from `fr-windows app`. Launch the real windowed
/// executable so its own viewer children and per-user installer keep their
/// existing executable identity; never copy or run the CLI as the GUI.
pub(super) fn launch(args: &[String]) -> ExitCode {
    if AppCommand::parse(args, DEFAULT_SERVICE_PORT).is_none() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let Ok(exe) = std::env::current_exe() else {
        eprintln!("fr-windows: could not locate the packaged desktop application.");
        return ExitCode::from(1);
    };
    let app = exe.with_file_name(crate::install_plan::APP_EXE);
    match Command::new(app).args(args).status() {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(_) => ExitCode::from(1),
        Err(_) => {
            eprintln!(
                "fr-windows: could not open FrankenRemote.exe. Keep it beside fr-windows.exe or reinstall the complete package."
            );
            ExitCode::from(1)
        }
    }
}

pub fn run() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(("--session", rest)) = args
        .split_first()
        .map(|(first, rest)| (first.as_str(), rest))
    {
        super::APPLICATION_SESSION.store(true, std::sync::atomic::Ordering::Relaxed);
        let code = super::run(rest);
        // The viewer had the last input; hand the foreground back so the
        // picker reappears in front.
        // SAFETY: a scalar foreground-permission grant.
        let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
        return code;
    }
    let quiet = args.iter().any(|arg| arg == "--quiet");
    match AppCommand::parse(&args, DEFAULT_SERVICE_PORT) {
        Some(AppCommand::Picker { port }) => application(port),
        Some(AppCommand::Install { quiet }) => install_command(quiet),
        Some(AppCommand::Uninstall { quiet }) => uninstall_command(quiet),
        None => {
            if !quiet {
                message(USAGE, MB_ICONINFORMATION);
            }
            ExitCode::from(2)
        }
    }
}

fn message(text: &str, style: MESSAGEBOX_STYLE) -> i32 {
    // SAFETY: owner-less message box; strings are copied during the call.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from(install::TITLE),
            style | MB_SETFOREGROUND,
        )
    }
    .0
}

fn install_command(quiet: bool) -> ExitCode {
    match install::install() {
        Ok(app) => {
            if !quiet {
                message(
                    &format!(
                        "FrankenRemote is installed for this user.\n\n{}\n\nOpen it from the Start menu. Remove it from Settings > Apps.",
                        app.display()
                    ),
                    MB_ICONINFORMATION | MB_OK,
                );
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if !quiet {
                message(
                    &format!("Installation failed: {error}"),
                    MB_ICONERROR | MB_OK,
                );
            }
            ExitCode::from(1)
        }
    }
}

fn uninstall_command(quiet: bool) -> ExitCode {
    if !quiet
        && message(
            "Remove FrankenRemote from this PC?\n\nThis removes the program, its Start menu shortcut and its saved choices. Tailscale is not changed.",
            MB_ICONQUESTION | MB_OKCANCEL | MB_DEFBUTTON2,
        ) != IDOK.0
    {
        return ExitCode::from(1);
    }
    match install::uninstall() {
        Ok(deferred) => {
            if !quiet {
                message("FrankenRemote was removed.", MB_ICONINFORMATION | MB_OK);
            }
            // The running copy's own files go last, once nothing waits on
            // this process.
            if let Some(dir) = deferred
                && let Err(error) = install::remove_later(&dir)
            {
                if !quiet {
                    message(
                        &format!("The program folder could not be removed: {error}"),
                        MB_ICONERROR | MB_OK,
                    );
                }
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        Err(error) => {
            if !quiet {
                message(&format!("Removal failed: {error}"), MB_ICONERROR | MB_OK);
            }
            ExitCode::from(1)
        }
    }
}

fn load_settings(path: Option<&Path>) -> Settings {
    path.and_then(|path| {
        let file = std::fs::File::open(path).ok()?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(
            &mut std::io::Read::take(file, crate::settings::MAX_BYTES as u64 + 1),
            &mut bytes,
        )
        .ok()?;
        Some(Settings::from_json(&bytes))
    })
    .unwrap_or_default()
}

fn save_settings(path: Option<&Path>, settings: &Settings) {
    let Some(path) = path else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Write-then-rename so a crash never leaves a torn document.
    let staged = path.with_extension("json.new");
    if std::fs::write(&staged, settings.to_json()).is_ok()
        && std::fs::rename(&staged, path).is_err()
    {
        let _ = std::fs::remove_file(&staged);
    }
}

fn application(port: NonZeroU16) -> ExitCode {
    // SAFETY: process-wide setup before any window exists; a manifest that
    // already set the same awareness makes the call fail harmlessly.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let controls = INITCOMMONCONTROLSEX {
            dwSize: u32::try_from(size_of::<INITCOMMONCONTROLSEX>()).unwrap_or(0),
            dwICC: ICC_LISTVIEW_CLASSES | ICC_STANDARD_CLASSES,
        };
        let _ = InitCommonControlsEx(&raw const controls);
    }
    // The capability probe initializes COM on its own thread so this UI
    // thread's apartment is never chosen by it.
    let support = std::thread::spawn(fr_mobile_ffi::video::decoder_support)
        .join()
        .ok();
    let settings_path = install::settings_path();
    let mut settings = load_settings(settings_path.as_deref());
    let mut installed = install::running_installed();
    let mut notice = None;
    loop {
        let request = Request {
            mode: Mode::Application,
            port: port.get(),
            settings: settings.clone(),
            notice: notice.take(),
            support: support.clone(),
            offer_install: !installed,
        };
        match hosts::pick(&request) {
            Ok(Picked::Closed) => return ExitCode::SUCCESS,
            Ok(Picked::Install) => match install::install() {
                Ok(app) => {
                    installed = true;
                    // SAFETY: a scalar foreground-permission grant.
                    let _ = unsafe { AllowSetForegroundWindow(ASFW_ANY) };
                    if install::launch(&app, port) {
                        return ExitCode::SUCCESS;
                    }
                    notice = Some(Notice {
                        text: format!(
                            "Installed to {}. Open FrankenRemote from the Start menu.",
                            app.display()
                        ),
                        warning: false,
                        sticky: true,
                    });
                }
                Err(error) => {
                    notice = Some(Notice {
                        text: format!("Installation failed: {error}"),
                        warning: true,
                        sticky: true,
                    });
                }
            },
            Ok(Picked::Machine(chosen)) => {
                settings = chosen.settings.clone();
                save_settings(settings_path.as_deref(), &settings);
                match session(chosen, port) {
                    Ok(result) => notice = Some(result),
                    Err(reason) => {
                        message(
                            &format!(
                                "{reason}\n\nFrankenRemote will close because the original viewer's outcome or cleanup could not be confirmed. Check the original session before reopening the app."
                            ),
                            MB_ICONERROR | MB_OK,
                        );
                        return ExitCode::from(1);
                    }
                }
            }
            Err(PickError::Choice(
                error @ (Error::NativeUnavailable | Error::CleanupIncomplete),
            )) => {
                message(
                    &format!("The FrankenRemote window could not be shown or closed ({error:?})."),
                    MB_ICONERROR | MB_OK,
                );
                return ExitCode::from(1);
            }
            Err(PickError::Choice(error)) => {
                notice = Some(Notice {
                    text: format!("That choice could not be used ({error:?}). Choose again."),
                    warning: true,
                    sticky: false,
                });
            }
            Err(PickError::Discovery(error)) => {
                notice = Some(Notice {
                    text: hosts::discovery_guidance(error).to_owned(),
                    warning: true,
                    sticky: false,
                });
            }
        }
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The path and command arguments never enter the content-free log.
fn begin_session_log() -> (Option<session_output::CappedLog>, bool) {
    let Some(path) = install::settings_path() else {
        return (None, true);
    };
    let opened = session_output::CappedLog::open(&path.with_file_name("sessions.log")).and_then(
        |mut log| {
            log.append(format!("=== session started at unix {}\n", unix_now()).as_bytes())?;
            Ok(log)
        },
    );
    match opened {
        Ok(log) => (Some(log), false),
        Err(_) => (None, true),
    }
}

/// Append only bounded diagnostics after both pipe readers have joined.
fn finish_session_log(output: &mut session_output::ChildOutput) -> bool {
    let failed = output
        .stderr
        .as_ref()
        .is_some_and(|capture| capture.log_error.is_some());
    let exit = output
        .status
        .and_then(|status| status.code())
        .map_or_else(|| "unavailable".to_owned(), |code| code.to_string());
    let mut text = format!("=== session ended at unix {}, exit {exit}\n", unix_now()).into_bytes();
    if output.failure.is_some() || output.cleanup_error.is_some() {
        text.extend_from_slice(
            format!(
                "[viewer monitor {:?}; cleanup {:?}]\n",
                output.failure, output.cleanup_error
            )
            .as_bytes(),
        );
    }
    if let Some(capture) = &output.stdout {
        if !capture.report.complete() {
            text.extend_from_slice(
                format!("[completion capture incomplete: {:?}]\n", capture.report).as_bytes(),
            );
        }
        // A retained document is diagnostic only. The picker separately checks
        // complete capture, the child's exit status, and the reported outcome.
        if let Some(document) = capture.retained_document() {
            text.extend_from_slice(document.as_bytes());
            text.push(b'\n');
        }
    }
    if let Some(capture) = &output.stderr
        && !capture.report.complete()
    {
        text.extend_from_slice(
            format!("[status capture incomplete: {:?}]\n", capture.report).as_bytes(),
        );
    }
    let write_failed = output
        .log
        .as_mut()
        .is_some_and(|log| log.append(&text).is_err());
    failed || write_failed
}

fn session(chosen: Chosen, port: NonZeroU16) -> Result<Notice, String> {
    let name = machines::short_name(chosen.peer.certificate_name()).to_owned();
    let failed = |text: String| Notice {
        text,
        warning: true,
        sticky: true,
    };
    let Ok(exe) = std::env::current_exe() else {
        return Ok(failed(
            "FrankenRemote could not find its own program file.".to_owned(),
        ));
    };
    // The locally selected receive path belongs only to this child. Keep it
    // out of the content-free session log and the remembered settings.
    let args = chosen
        .settings
        .viewer_args(chosen.peer.stable_id(), port, chosen.receive);
    let child = Command::new(exe)
        .arg("--session")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => return Ok(failed(format!("Could not start the viewer ({error})."))),
    };
    // SAFETY: a scalar foreground-permission grant for the new viewer.
    let _ = unsafe { AllowSetForegroundWindow(child.id()) };
    let (log, open_failed) = begin_session_log();
    let mut output = session_output::follow_child(child, log);
    let write_failed = finish_session_log(&mut output);
    let summary = session_output::summarize(&output, &name, open_failed || write_failed);
    if summary.continuation == Continuation::Stop {
        return Err(summary.message);
    }
    Ok(Notice {
        text: summary.message,
        warning: !summary.ok,
        sticky: true,
    })
}
