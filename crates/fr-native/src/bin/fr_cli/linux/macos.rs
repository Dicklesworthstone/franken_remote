//! `fr connect` on macOS (fr-5a4w): the same tailnet selection, trust roots,
//! offers and reconnect supervisor as Linux, with the `AppKit`/Metal session
//! from `fr_native::macos::session` in place of the X11 desktop.
//!
//! Not implemented on macOS yet, and refused by name rather than ignored:
//! interactive file drops, audio, microphone,
//! and system-shortcut capture. Local inspection and disconnect address the same
//! authenticated foreground owner and wait for its actual native cleanup.
//! Explicit --send/--receive choices and opt-in local upload commands use the
//! original controlled file lanes.
#[path = "macos_clipboard.rs"]
mod clipboard;
#[path = "macos_files.rs"]
mod files;
#[path = "macos_history.rs"]
mod history;
#[path = "macos_host_choice.rs"]
mod host_choice;
use super::{
    Failure, Shutdown, failure, lapse, read_roots, refusal, robot, session_control, tailnet,
};
use crate::options::{Connection, DisplayChoice};
use asupersync::{cx::Cx, runtime::Runtime, types::Budget};
use fr_native::macos::session::{ClipboardReport, ControlPolicy, LocalStop, Report, Session};
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
    if !connection.control && !connection.upload.paths.is_empty() {
        return Err(Failure::new(
            "send_requires_control",
            "Files are sent only on your own control lease; pass --control with --send.",
            2,
        ));
    }
    if !connection.control && connection.receive.is_some() {
        return Err(Failure::new(
            "receive_requires_control",
            "Files are received only on your own control lease; pass --control with --receive.",
            2,
        ));
    }
    if !connection.control && connection.upload.file_commands {
        return Err(Failure::new(
            "file_commands_require_control",
            "Local file commands use the original upload control lease; pass --control with --file-commands. A viewing reconnect never re-enables them.",
            2,
        ));
    }
    let directions =
        u32::from(!connection.upload.paths.is_empty() || connection.upload.file_commands)
            + u32::from(connection.receive.is_some());
    let clipboard = u32::from(connection.control && connection.clipboard);
    let sharing_width = 152 + 230 * directions + 184 * clipboard;
    let minimum_width = if connection.control {
        sharing_width.max(480)
    } else {
        sharing_width
    };
    let minimum_height = if connection.control { 184 } else { 108 };
    if connection.fit_window.is_some_and(|(width, height)| {
        width < minimum_width || height < minimum_height
    }) {
        return Err(Failure::new(
            "viewer_fit_too_small",
            "--fit must leave room for Change Display and visible sharing controls. Controlling windows also reserve the local text composer: minimum 480x184 points. Width is at least 152 plus 230 per file direction and 184 for the default-on clipboard; both file directions with clipboard need 796x184. Viewing-only windows need 152x108. --no-clipboard removes only the clipboard strip.",
            2,
        ));
    }
    let refused = if connection.capture_system_shortcuts {
        Some("--capture-system-shortcuts is not implemented on macOS yet.")
    } else if connection.audio.is_some() {
        Some("--audio is not implemented on macOS yet.")
    } else if connection.microphone.is_some() {
        Some("--microphone is not implemented on macOS yet.")
    } else if connection.upload.file_drop {
        Some(
            "--file-drop is not implemented on macOS yet; use --file-commands for later explicit selections, or choose --send paths and/or a --receive directory before connecting.",
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
    // Pin the user's explicit sources and destination before AppKit, host
    // lookup or network I/O. These choices belong to one original control.
    let selected_files = files::select(connection)?;
    let pending = files::Summary::capture(&selected_files);
    let pending_clipboard = ClipboardReport {
        requested: connection.control && connection.clipboard,
        ..ClipboardReport::default()
    };
    connect_selected(
        runtime,
        cx,
        shutdown,
        stopped,
        api,
        connection,
        json,
        selected_files,
    )
    .map_err(|mut error| {
        pending.preserve(&mut error);
        clipboard::preserve(&pending_clipboard, &mut error);
        error
    })
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn connect_selected(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    connection: &Connection,
    json: bool,
    selected_files: fr_native::macos::files::Files,
) -> Result<String, Failure> {
    fr_native::macos::initialize_application().map_err(|_| {
        failure(
            "gui_unavailable",
            "macOS refused to start the application (run fr from a logged-in graphical session, on the main thread).",
        )
    })?;
    let selected = if connection.choose_host {
        host_choice::select(runtime, cx, shutdown, stopped, &api, connection, json)?
    } else {
        crate::saved_hosts::select(&connection.target)?
    };
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
    )
    .with_fit_window(connection.fit_window)
    .with_files(selected_files)
    .clipboard(connection.control && connection.clipboard);
    if display == DisplayChoice::Choose {
        session = session.with_display_picker();
    }
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
    // This context remains live while cancellation drains the original
    // connection. Its requester receives cleanup evidence from that owner.
    let local = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
        .map_err(|_| failure("runtime_unavailable", "Check local runtime resources."))?;
    session = session.with_cleanup_context(local.clone());
    let mut owner = runtime
        .block_on(session_control::Owner::bind(&local, &selected.node))
        .map_err(robot::local_failure)?;
    session
        .file_command_connection(&owner.info().instance)
        .map_err(crate::file_commands::local_failure)?;
    let selector = selected.selector();
    let result = if connection.control {
        // Drop the Ref before handing the mutable session to its supervisor.
        // Clipboard and file consent are independent original choices.
        let offer = session.files().offer(connection.clipboard);
        let operation = client.run_control_capable(
            cx.clone(),
            runtime.handle(),
            selector,
            cfg,
            offer,
            policy,
            &mut session,
        );
        runtime.block_on(shutdown.run_connection(cx, stopped, operation, &local, &mut owner))
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
        runtime.block_on(shutdown.run_connection(cx, stopped, operation, &local, &mut owner))
    };
    let cleanup = if session.cleanup_confirmed()
        && !matches!(
            result,
            Err(reconnect::Failure::Cleanup | reconnect::Failure::CleanupExpired)
        ) {
        session_control::Completion::Confirmed
    } else {
        session_control::Completion::Unconfirmed
    };
    // Losing a requester cannot undo cleanup or fabricate a reply. The caller
    // independently reports an unknown outcome if this response is lost.
    let _ = runtime.block_on(owner.complete(&local, cleanup));
    let evidence = files::Summary::capture(&session.files());
    evidence.finish(
        finish(
            &session.report(),
            result,
            stopped.get(),
            json,
            session.cleanup_confirmed(),
            reconnect::Application::terminal_failure(&session),
            session.picker_error(),
        ),
        json,
    )
}

const fn stop_name(stop: LocalStop) -> &'static str {
    match stop {
        LocalStop::WindowClosed => "window_closed",
        LocalStop::LocalEscape => "local_escape",
        LocalStop::FocusLost => "focus_lost",
        LocalStop::WindowHidden => "window_hidden",
        LocalStop::PictureChanged => "picture_changed",
        LocalStop::Overflow => "input_overflow",
        LocalStop::NativeFailure => "native_failure",
    }
}

// Four independent observed facts, passed unchanged to one projection.
#[allow(clippy::fn_params_excessive_bools, clippy::too_many_arguments)]
fn finish(
    report: &Report,
    result: Result<(), reconnect::Failure>,
    stopped: bool,
    json: bool,
    cleanup_confirmed: bool,
    terminal: Option<frd::session_startup::ObserverError>,
    picker_error: Option<fr_native::macos::picker::Error>,
) -> Result<String, Failure> {
    let result = crate::closure::preserve(
        finish_result(
            report,
            result,
            stopped,
            json,
            cleanup_confirmed,
            terminal,
            picker_error,
        ),
        report.close_exchange,
    )
    .map_err(|mut error| {
        if let Some(revocation) = report.lease_revocation {
            error.revocation = crate::terminal::failure(revocation).revocation;
        }
        error
    });
    clipboard::finish(
        &report.clipboard,
        history::finish(report, result, json),
        json,
    )
}

// Keep the ordered failure precedence beside its single completion projection.
#[allow(
    clippy::too_many_lines,
    clippy::fn_params_excessive_bools,
    clippy::too_many_arguments
)]
fn finish_result(
    report: &Report,
    result: Result<(), reconnect::Failure>,
    stopped: bool,
    json: bool,
    cleanup_confirmed: bool,
    terminal: Option<frd::session_startup::ObserverError>,
    picker_error: Option<fr_native::macos::picker::Error>,
) -> Result<String, Failure> {
    let closed = matches!(
        report.local_stop,
        Some(LocalStop::WindowClosed | LocalStop::LocalEscape)
    );
    let result = super::macos_completion::checked(result, closed, cleanup_confirmed, terminal);
    if let Err(error) = result {
        if matches!(
            error,
            reconnect::Failure::Cleanup | reconnect::Failure::CleanupExpired
        ) {
            let mut failed = failure(
                "native_cleanup_incomplete",
                "Native cleanup was not confirmed; inspect the original local viewer before another connection. A window close or interruption does not confirm cleanup.",
            );
            if let Some(reported) = terminal.and_then(refusal::observation) {
                failed.closure = reported.closure;
                failed.revocation = reported.revocation;
            }
            return Err(failed);
        }
        // Authenticated host refusal/revocation wins over a racing local close,
        // focus event or signal. Keep its original cleanup/effect report.
        if super::macos_completion::requested_cleanup_failed(error) {
            return Err(failure(
                "host_cleanup_incomplete",
                "The host acknowledged the requested close but reported failed cleanup. Inspect the retained host cleanup and effect stages before reconnecting; previously submitted actions were not rolled back.",
            ));
        }
        if let Some(named) = refusal::reconnect(error).or_else(|| lapse::reconnect(error)) {
            return Err(named);
        }
        if stopped {
            return Err(Failure::new(
                "cancelled",
                "The session was interrupted locally; no actions were replayed.",
                130,
            ));
        }
        if matches!(
            error,
            reconnect::Failure::Observation(frd::session_startup::ObserverError::Application)
        ) && let Some(error) = picker_error
        {
            return Err(display_choice_failure(error));
        }
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
                _ => match error {
                        reconnect::Failure::Connection(frd::native_connection::Error::Tailnet(
                            e,
                        )) => Some(tailnet(e)),
                        _ => None,
                    }
                    .unwrap_or_else(|| {
                        // The typed chain is content-free (no addresses, names or
                        // payloads), so an unnamed end stays diagnosable on stderr.
                        eprintln!("fr: session ended without a named reason: {error:?}");
                        failure(
                            "native_session_failed",
                            "The native session ended with a failure; check fr hosts and that frd run is serving on the host, then reconnect.",
                        )
                    }),
            });
    }
    if stopped {
        return Err(Failure::new(
            "cancelled",
            "The session was interrupted locally and native cleanup completed; no actions were replayed.",
            130,
        ));
    }
    let ended = report.local_stop.map_or("host_or_session_end", stop_name);
    let hardware = report
        .hardware_decode
        .map_or("unknown", |h| if h { "true" } else { "false" });
    let close = report.close_exchange.map(crate::closure::Report::from);
    let mut out = String::new();
    if json {
        let _ = writeln!(
            out,
            "{{\"outcome\":\"success\",\"platform\":\"macos\",\"attempts\":{},\"viewing_reconnects\":{},\"opened\":{},\"presented\":{},\"hardware_decode\":{},\"approval_requested\":{},\"control\":{{\"requested\":{},\"requests\":{},\"granted\":{},\"keys\":{},\"pointer\":{},\"buttons\":{},\"scrolls\":{},\"results\":{}}},\"ended\":\"{}\",\"local_cleanup_confirmed\":true,\"close_exchange\":{},\"lease_revocation\":{}}}",
            report.attempts,
            report.viewing_reconnects,
            report.opened,
            report.presented,
            report
                .hardware_decode
                .map_or("null", |h| if h { "true" } else { "false" }),
            report.approval_requested,
            report.control_requested,
            report.control_requests,
            report.control_granted,
            report.keys,
            report.pointer,
            report.buttons,
            report.scrolls,
            report.input_results,
            ended,
            close.map_or_else(|| "null".to_owned(), crate::closure::Report::json),
            crate::terminal::lease_json(report.lease_revocation),
        );
    } else {
        let _ = writeln!(
            out,
            "Session ended ({ended}). Attempts: {}; viewing reconnects: {}; windows opened: {}; frames presented: {}; hardware decode: {hardware}. Local cleanup confirmed.",
            report.attempts, report.viewing_reconnects, report.opened, report.presented,
        );
        if report.control_requested {
            let _ = writeln!(
                out,
                "Control: {}; sent keys {}, pointer moves {}, buttons {}, scrolls {}; host results {}.",
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
        let _ = writeln!(out, "{}", close.map_or_else(
            || "No host closing exchange was collected; remote cleanup and effects are unknown.".to_owned(),
            crate::closure::Report::text,
        ));
        if let Some(revocation) = report.lease_revocation {
            let _ = writeln!(
                out,
                "Host input lease: {}.",
                crate::terminal::lease_text(revocation)
            );
        }
    }
    Ok(out)
}

fn display_choice_failure(error: fr_native::macos::picker::Error) -> Failure {
    use fr_native::macos::picker::Error;
    match error {
        Error::Cancelled => Failure::new(
            "display_choice_cancelled",
            "Display selection was cancelled locally and local cleanup completed; earlier host cleanup and file receipts remain in this result.",
            130,
        ),
        Error::ChoiceTimeout => failure(
            "display_choice_expired",
            "The original display-choice deadline expired. Reconnect to obtain a fresh approved display catalog.",
        ),
        Error::CatalogChanged => failure(
            "display_catalog_changed",
            "The approved display catalog changed while choosing. The old choice was discarded; reconnect for a fresh catalog.",
        ),
        Error::EmptyCatalog => failure(
            "display_choice_empty",
            "The approved host catalog has no selectable display. Check the host's interactive session and display permissions.",
        ),
        Error::EventFlood => failure(
            "display_choice_event_overflow",
            "The local picker could not finish its bounded event batch. No queued selection was accepted; make a fresh connection and choice.",
        ),
        _ => failure(
            "display_picker_unavailable",
            "The native display picker could not complete a valid selection. Run from the logged-in graphical session and reconnect.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{Command, Options};

    fn connection(extra: &str) -> Connection {
        let Options {
            command: Command::Connect(connection),
            ..
        } = crate::options::parse(
            &format!("connect n-host --control --experimental-native --display only {extra}")
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
        .unwrap()
        else {
            panic!("connection required")
        };
        connection
    }

    #[test]
    fn explicit_file_directions_are_admitted_without_implicit_drop_or_clipboard_support() {
        for flags in [
            "--send /private/chosen",
            "--receive /private/inbox",
            "--send /private/chosen --receive /private/inbox",
        ] {
            let connection = connection(flags);
            assert!(check(&connection).is_ok());
            assert!(!connection.upload.file_drop && !connection.upload.file_commands);
        }
        assert_eq!(
            check(&connection("--file-drop")).err().unwrap().code,
            "platform_unavailable"
        );
        for flags in [
            "--file-commands",
            "--file-commands --send /private/chosen",
            "--file-commands --receive /private/inbox",
            "--file-commands --send /private/chosen --receive /private/inbox",
        ] {
            let connection = connection(flags);
            assert!(check(&connection).is_ok());
            assert!(connection.upload.file_commands && !connection.upload.file_drop);
        }
    }

    #[test]
    fn programmatic_file_requests_require_control_and_fit_keeps_cancel_controls_available() {
        let mut upload = connection("--send /private/chosen");
        upload.control = false;
        assert_eq!(check(&upload).err().unwrap().code, "send_requires_control");
        let mut download = connection("--receive /private/inbox");
        download.control = false;
        assert_eq!(
            check(&download).err().unwrap().code,
            "receive_requires_control"
        );
        let mut commands = connection("--file-commands");
        commands.control = false;
        assert_eq!(
            check(&commands).err().unwrap().code,
            "file_commands_require_control"
        );
        for (flags, width, height) in [
            ("", 479, 184),
            ("", 480, 183),
            ("--send /private/chosen", 565, 184),
            ("--file-commands", 565, 184),
            ("--receive /private/inbox", 566, 183),
            ("--send /private/chosen --receive /private/inbox", 795, 184),
            ("--file-commands --receive /private/inbox", 795, 184),
        ] {
            let mut c = connection(flags);
            c.fit_window = Some((width, height));
            assert_eq!(check(&c).err().unwrap().code, "viewer_fit_too_small");
        }
        let mut c = connection("--no-clipboard --send /private/chosen --receive /private/inbox");
        c.fit_window = Some((612, 184));
        assert!(check(&c).is_ok());
    }

    #[test]
    fn clipboard_privacy_control_has_space_independently_of_file_controls() {
        for (flags, minimum) in [
            ("", 480),
            ("--file-commands", 566),
            ("--receive /private/inbox", 566),
            ("--file-commands --receive /private/inbox", 796),
            ("--no-clipboard", 480),
        ] {
            let mut c = connection(flags);
            c.fit_window = Some((minimum - 1, 184));
            assert_eq!(check(&c).err().unwrap().code, "viewer_fit_too_small");
            c.fit_window = Some((minimum, 184));
            assert!(check(&c).is_ok());
        }
        let mut viewing = connection("");
        viewing.control = false;
        viewing.fit_window = Some((152, 108));
        assert!(check(&viewing).is_ok());
    }

    #[test]
    fn native_host_and_display_choices_pass_platform_preflight() {
        let mut c = connection("");
        c.display = DisplayChoice::Choose;
        assert!(check(&c).is_ok());
        c.choose_host = true;
        c.target.node.clear();
        assert!(check(&c).is_ok());
    }
}
