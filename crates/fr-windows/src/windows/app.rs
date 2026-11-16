//! The `FrankenRemote` desktop application: the machine picker, one viewer
//! session at a time, and back to the picker with a plain-language outcome.
//!
//! Each session runs in a child process of this same executable
//! (`--session view <stable-id> ...`), which is exactly the command-line
//! viewer: a fresh installed-Tailscale lookup resolves the chosen stable ID
//! before dialing, and every session's native state ends with its process.
//! The child's content-free completion document becomes the picker notice.
use super::hosts::{self, Chosen, Error, Mode, Notice, PickError, Picked, Request};
use super::install;
use crate::{machines, outcome, settings::Settings};
use std::{
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
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--quiet")
        .collect();
    match rest.as_slice() {
        [] => application(),
        ["--install"] => install_command(quiet),
        ["--uninstall"] => uninstall_command(quiet),
        _ => {
            if !quiet {
                message(
                    "Usage: FrankenRemote [--install | --uninstall] [--quiet]\n\nRun without arguments to choose a machine. The command-line viewer is fr-windows.exe.",
                    MB_ICONINFORMATION,
                );
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

fn application() -> ExitCode {
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
                    if install::launch(&app) {
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
                notice = Some(session(&chosen));
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

fn session(chosen: &Chosen) -> Notice {
    let name = machines::short_name(chosen.peer.certificate_name()).to_owned();
    let failed = |text: String| Notice {
        text,
        warning: true,
        sticky: true,
    };
    let Ok(exe) = std::env::current_exe() else {
        return failed("FrankenRemote could not find its own program file.".to_owned());
    };
    let child = Command::new(exe)
        .arg("--session")
        .args(chosen.settings.viewer_args(chosen.peer.stable_id()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(0x0800_0000)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(error) => return failed(format!("Could not start the viewer ({error}).")),
    };
    // SAFETY: a scalar foreground-permission grant for the new viewer.
    let _ = unsafe { AllowSetForegroundWindow(child.id()) };
    let output = match child.wait_with_output() {
        Ok(output) => output,
        Err(error) => return failed(format!("The viewer could not be followed ({error}).")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(document) = stdout
        .lines()
        .rev()
        .find(|line| line.trim_start().starts_with('{'))
    {
        let summary = outcome::summarize(document, &name);
        return Notice {
            text: summary.message,
            warning: !summary.ok,
            sticky: true,
        };
    }
    // No completion document: the command line refused before a session
    // (its stderr line is content-free guidance).
    let stderr = String::from_utf8_lossy(&output.stderr);
    let reason = stderr
        .lines()
        .find(|line| !line.trim().is_empty())
        .map_or_else(
            || format!("exit {}", output.status.code().unwrap_or(-1)),
            |line| line.trim_start_matches("fr-windows: ").trim().to_owned(),
        );
    failed(format!("Could not connect to {name}: {reason}"))
}
