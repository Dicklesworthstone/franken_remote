//! `fr connect` on macOS (fr-5a4w): the same tailnet selection, trust roots,
//! offers and reconnect supervisor as Linux, with the `AppKit`/Metal session
//! from `fr_native::macos::session` in place of the X11 desktop.
//!
//! Not implemented on macOS yet, and refused by name rather than ignored:
//! file transfer, audio, microphone, the native host/display pickers, `--fit`,
//! system-shortcut capture and the clipboard (whose default-on choice is
//! reported as unavailable). The local session-control socket (`fr status`,
//! `fr disconnect` against a running viewer) is not bound by this path.
use super::{Failure, Shutdown, failure, lapse, read_roots, refusal};
use crate::options::{Connection, DisplayChoice};
use asupersync::{cx::Cx, runtime::Runtime};
use fr_native::macos::session::{ControlPolicy, LocalStop, Report, Session};
use frd::{
    native_connection::{AddressFamily, Client, Configuration, LocalApi, reconnect},
    session_startup::ObserverPolicy,
};
use std::{cell::Cell, fmt::Write as _, time::Duration};

fn unsupported(option: &'static str) -> Failure {
    Failure::new("platform_unavailable", option, 2)
}

/// Refuse every option this path does not implement, before any network I/O.
fn check(connection: &Connection) -> Result<(), Failure> {
    let refused = if connection.choose_host {
        Some("--choose-host is not implemented on macOS yet; name the host instead.")
    } else if connection.display == DisplayChoice::Choose {
        Some(
            "--display choose is not implemented on macOS yet; use --display only or a handle from fr displays.",
        )
    } else if connection.fit_window.is_some() {
        Some(
            "--fit is not implemented on macOS; the window opens at the remote size, shrunk to the screen.",
        )
    } else if connection.capture_system_shortcuts {
        Some("--capture-system-shortcuts is not implemented on macOS yet.")
    } else if connection.audio.is_some() {
        Some("--audio is not implemented on macOS yet.")
    } else if connection.microphone.is_some() {
        Some("--microphone is not implemented on macOS yet.")
    } else if connection.receive.is_some()
        || !connection.upload.paths.is_empty()
        || connection.upload.file_drop
        || connection.upload.file_commands
    {
        Some(
            "File transfer (--send, --file-drop, --file-commands, --receive) is not implemented on macOS yet.",
        )
    } else {
        None
    };
    refused.map_or(Ok(()), |next| Err(unsupported(next)))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn connect(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    connection: &Connection,
    json: bool,
    automation: bool,
) -> Result<String, Failure> {
    if automation {
        return Err(unsupported(
            "Foreground automation (fr robot session open) is not implemented on macOS yet.",
        ));
    }
    check(connection)?;
    fr_native::macos::initialize_application().map_err(|_| {
        failure(
            "gui_unavailable",
            "macOS refused to start the application (run fr from a logged-in graphical session, on the main thread).",
        )
    })?;
    let selected = crate::saved_hosts::select(&connection.target)?;
    let roots = read_roots(&connection.target.roots)?;
    let mut client = Client::new(api, roots, Duration::from_secs(5)).map_err(|_| {
        failure(
            "invalid_native_configuration",
            "Verify the locally provisioned trust store and native connection policy.",
        )
    })?;
    let mut epoch = [0_u8; 16];
    cx.random_bytes(&mut epoch);
    let epoch = u128::from_be_bytes(epoch).max(1);
    let display = connection.display;
    let choose = Box::new(move |catalog: &fr_wire::display::Catalog| {
        display.select(catalog).map(Some).ok_or(())
    });
    let mut session = Session::new(
        connection.control.then(ControlPolicy::default),
        ObserverPolicy::default(),
        choose,
        epoch,
        format!("FrankenRemote: {}", selected.node),
    );
    let cfg = Configuration {
        port: selected.port,
        family: if connection.target.ipv6 {
            AddressFamily::Ipv6
        } else {
            AddressFamily::Automatic
        },
        ..Default::default()
    };
    let policy = reconnect::Policy {
        max_attempts: connection.attempts,
        ..Default::default()
    };
    let selector = selected.selector();
    let result = if connection.control {
        let operation = client.run_control_capable(
            cx.clone(),
            runtime.handle(),
            selector,
            cfg,
            fr_client::native::control_offer(),
            policy,
            &mut session,
        );
        runtime.block_on(shutdown.run(cx, stopped, operation))
    } else {
        let operation = client.run_observing(
            cx.clone(),
            runtime.handle(),
            selector,
            cfg,
            fr_client::native::observation_offer(),
            policy,
            &mut session,
        );
        runtime.block_on(shutdown.run(cx, stopped, operation))
    };
    finish(
        &session.report(),
        result,
        stopped.get(),
        connection.control && connection.clipboard,
        json,
    )
}

const fn stop_name(stop: LocalStop) -> &'static str {
    match stop {
        LocalStop::WindowClosed => "window_closed",
        LocalStop::FocusLost => "focus_lost",
        LocalStop::WindowHidden => "window_hidden",
        LocalStop::PictureChanged => "picture_changed",
        LocalStop::Overflow => "input_overflow",
        LocalStop::NativeFailure => "native_failure",
    }
}

fn finish(
    report: &Report,
    result: Result<(), reconnect::Failure>,
    stopped: bool,
    clipboard_requested: bool,
    json: bool,
) -> Result<String, Failure> {
    let closed = report.local_stop == Some(LocalStop::WindowClosed);
    if let Err(error) = result {
        if stopped {
            return Err(Failure::new(
                "cancelled",
                "The session was interrupted locally; no actions were replayed.",
                130,
            ));
        }
        // A user's own window close is the ordinary end of a session.
        if !closed {
            return Err(match report.local_stop {
                Some(LocalStop::FocusLost) => failure(
                    "control_focus_lost",
                    "The viewer window lost keyboard focus, so local input capture stopped and control ended; keep the FrankenRemote window focused and reconnect. Nothing is replayed.",
                ),
                Some(LocalStop::WindowHidden) => failure(
                    "control_window_changed",
                    "The viewer window was hidden, minimized or covered, so control ended; reconnect to acquire control again.",
                ),
                Some(LocalStop::PictureChanged) => failure(
                    "control_window_resized",
                    "The viewer's picture changed size (for example, moved to a display with another scale), so its input mapping and control ended; reconnect.",
                ),
                _ => refusal::reconnect(error)
                    .or_else(|| lapse::reconnect(error))
                    .unwrap_or_else(|| {
                        failure(
                            "native_session_failed",
                            "The native session ended with a failure; check fr hosts and that frd run is serving on the host, then reconnect.",
                        )
                    }),
            });
        }
    }
    let ended = report.local_stop.map_or("host_or_session_end", stop_name);
    let hardware = report
        .hardware_decode
        .map_or("unknown", |h| if h { "true" } else { "false" });
    let clipboard = if clipboard_requested {
        "local_clipboard_unavailable"
    } else {
        "not_requested"
    };
    let mut out = String::new();
    if json {
        let _ = writeln!(
            out,
            "{{\"outcome\":\"success\",\"platform\":\"macos\",\"attempts\":{},\"opened\":{},\"presented\":{},\"hardware_decode\":{},\"approval_requested\":{},\"control\":{{\"requested\":{},\"granted\":{},\"keys\":{},\"pointer\":{},\"buttons\":{},\"scrolls\":{},\"results\":{}}},\"clipboard\":\"{}\",\"ended\":\"{}\"}}",
            report.attempts,
            report.opened,
            report.presented,
            report
                .hardware_decode
                .map_or("null", |h| if h { "true" } else { "false" }),
            report.approval_requested,
            report.control_requested,
            report.control_granted,
            report.keys,
            report.pointer,
            report.buttons,
            report.scrolls,
            report.input_results,
            clipboard,
            ended,
        );
    } else {
        let _ = writeln!(
            out,
            "Session ended ({ended}). Attempts: {}; windows opened: {}; frames presented: {}; hardware decode: {hardware}.",
            report.attempts, report.opened, report.presented,
        );
        if report.control_requested {
            let _ = writeln!(
                out,
                "Control: {}; sent keys {}, pointer moves {}, buttons {}, scrolls {}; host results {}. Clipboard: {clipboard}.",
                if report.control_granted {
                    "granted"
                } else {
                    "requested, not granted"
                },
                report.keys,
                report.pointer,
                report.buttons,
                report.scrolls,
                report.input_results,
            );
        }
    }
    Ok(out)
}
