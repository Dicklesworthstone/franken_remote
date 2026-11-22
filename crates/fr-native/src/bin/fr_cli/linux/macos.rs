//! `fr connect` on macOS (fr-5a4w): the same tailnet selection, trust roots,
//! offers and reconnect supervisor as Linux, with the `AppKit`/Metal session
//! from `fr_native::macos::session` in place of the X11 desktop.
//!
//! Not implemented on macOS yet, and refused by name rather than ignored:
//! system-shortcut capture. Local inspection and disconnect address the same
//! authenticated foreground owner and wait for its actual native cleanup.
//! Explicit --send/--receive choices, --file-drop native selections and opt-in
//! local upload commands use the original controlled file lanes.
#[path = "macos_clipboard.rs"]
mod clipboard;
#[path = "macos_file_choice.rs"]
mod file_choice;
#[path = "macos_files.rs"]
mod files;
#[path = "macos_history.rs"]
mod history;
#[path = "macos_host_choice.rs"]
mod host_choice;
#[path = "macos_media.rs"]
mod media;
#[path = "macos_microphone.rs"]
mod microphone;
use super::{
    Failure, Shutdown, failure, lapse, macos_audio as playback, read_roots, refusal, robot,
    session_control, tailnet,
};
use crate::{
    options::{Connection, DisplayChoice},
    robot_observe::Minimum,
};
use asupersync::{cx::Cx, runtime::Runtime, types::Budget};
use fr_native::{
    macos::session::{ClipboardReport, ControlPolicy, LocalStop, Report, Session},
    macos_live_observation::Event,
};
use frd::{
    native_connection::{AddressFamily, Client, Configuration, LocalApi, reconnect},
    session_startup::ObserverPolicy,
};
use std::{cell::Cell, fmt::Write as _, rc::Rc, time::Duration};

fn unsupported(option: &'static str) -> Failure {
    Failure::new("platform_unavailable", option, 2)
}

/// One explicit view publishes through its existing authenticated IPC owner.
/// This contains no screenshot handle, input authority, or replacement session.
#[derive(Clone)]
struct Publication {
    publisher: session_control::observation::Publisher,
    minimum: Minimum,
    insufficient: Rc<Cell<bool>>,
    reopen_refused: Rc<Cell<bool>>,
}
impl Publication {
    fn publish(&self, event: Event) -> Result<(), ()> {
        match event {
            Event::Begin(1) => self.publisher.begin(1),
            Event::Begin(_) => {
                // The supervisor's retry budget does not count explicit
                // display choices or resize reopens. Connecting is notified
                // before lookup/dial: refuse any second owner here as well.
                self.reopen_refused.set(true);
                self.publisher
                    .retire(fr_client::robot::live::Phase::Stopped);
                Err(())
            }
            Event::Retire(phase) => {
                self.publisher.retire(phase);
                Ok(())
            }
            Event::Ready { evidence, .. }
                if self.minimum == Minimum::SubmittedToCompositor
                    && evidence != fr_client::robot::live::Evidence::SubmittedToCompositor =>
            {
                self.insufficient.set(true);
                self.publisher
                    .retire(fr_client::robot::live::Phase::Stopped);
                Err(())
            }
            event => self.publisher.update(|state| event.apply(state)),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn observe(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    connection: &Connection,
    json: bool,
    minimum: Minimum,
) -> Result<String, Failure> {
    // The public grammar already fixes this profile; keep the owner boundary
    // explicit so a future caller cannot add authority or implicit selections.
    if connection.control
        || connection.attempts != 1
        || connection.choose_host
        || connection.display == DisplayChoice::Choose
        || connection.audio.is_some()
        || connection.microphone.is_some()
        || connection.receive.is_some()
        || connection.choose_receive_directory
        || !connection.upload.paths.is_empty()
        || connection.upload.choose_files
        || connection.upload.file_commands
        || connection.upload.file_drop
    {
        return Err(Failure::new(
            "observation_profile_invalid",
            "Publish observations only from an explicit foreground view of one selected display, with one attempt and no auxiliary sharing.",
            2,
        ));
    }
    let publication = Publication {
        publisher: session_control::observation::Publisher::default(),
        minimum,
        insufficient: Rc::new(Cell::new(false)),
        reopen_refused: Rc::new(Cell::new(false)),
    };
    connect_profile(
        runtime,
        cx,
        shutdown,
        stopped,
        api,
        connection,
        json,
        Some(&publication),
    )
    .map_err(|mut error| {
        // Preserve actual cleanup, host refusal and closure evidence. Only the
        // otherwise generic native failure is refined by this exact callback.
        if publication.insufficient.get() && error.code == "native_session_failed" {
            error.code = "observation_evidence_insufficient";
            error.next = "The actual native receipt proved decoding only, below the requested compositor-submission minimum. The original view ended; no stronger evidence or retry was inferred.";
        } else if publication.reopen_refused.get()
            && matches!(
                error.code,
                "native_session_failed" | "control_window_resized"
            )
        {
            error.code = "observation_attempt_limit";
            error.next = "The original observation ended after a display or window change. This explicit publisher permits one attempt; open a new foreground view for the new selection. No replacement connection was opened.";
        }
        error
    })
}

/// Refuse every option this path does not implement, before any network I/O.
fn check(connection: &Connection) -> Result<(), Failure> {
    playback::check(connection.audio.as_ref(), cfg!(feature = "macos-audio"))?;
    microphone::check(connection.microphone.as_ref())?;
    if connection.upload.choose_files
        && (!connection.control || !connection.upload.paths.is_empty())
    {
        return Err(Failure::new(
            "file_choice_invalid",
            "Use --choose-files only with --control and without --send paths.",
            2,
        ));
    }
    if !connection.control && !connection.upload.paths.is_empty() {
        return Err(Failure::new(
            "send_requires_control",
            "Files are sent only on your own control lease; pass --control with --send.",
            2,
        ));
    }
    if !connection.control && (connection.receive.is_some() || connection.choose_receive_directory)
    {
        return Err(Failure::new(
            "receive_requires_control",
            "Files are received only on your own control lease; pass --control with --receive or --choose-receive-directory.",
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
    if !connection.control && connection.upload.file_drop {
        return Err(Failure::new(
            "file_drop_requires_control",
            "Native file selections use the original upload control lease; pass --control with --file-drop. A viewing reconnect never re-enables them.",
            2,
        ));
    }
    if connection.choose_receive_directory
        && (connection.receive.is_some() || connection.pick.is_some())
    {
        return Err(Failure::new(
            "receive_directory_choice_invalid",
            "Choose one receive destination only in the original controlling viewer, never in a selection-only picker.",
            2,
        ));
    }
    let directions =
        u32::from(
            !connection.upload.paths.is_empty()
                || connection.upload.choose_files
                || connection.upload.file_commands
                || connection.upload.file_drop,
        ) + u32::from(connection.receive.is_some() || connection.choose_receive_directory);
    let clipboard = u32::from(connection.control && connection.clipboard);
    playback::check_profile(
        connection.control,
        clipboard != 0,
        directions,
        connection.audio.is_some(),
    )?;
    microphone::check_profile(
        clipboard != 0,
        directions,
        connection.audio.is_some(),
        connection.microphone.is_some(),
    )?;
    let sharing_width =
        152 + 230 * directions + 184 * clipboard + 144 * u32::from(connection.upload.file_drop);
    let mut minimum_width = if connection.control {
        sharing_width.max(480)
    } else {
        sharing_width
    };
    let mut minimum_height = if connection.control { 184 } else { 108 };
    if connection.audio.is_some() {
        minimum_width = minimum_width.max(320);
        minimum_height += 44;
    }
    if connection
        .fit_window
        .is_some_and(|(width, height)| width < minimum_width || height < minimum_height)
    {
        return Err(Failure::new(
            "viewer_fit_too_small",
            "--fit must leave room for Change Display and visible sharing controls. Controlling windows reserve the text composer: minimum 480x184 points. Width is at least 152 plus 230 per file direction, 184 for clipboard and 144 for --file-drop; both file directions with clipboard and file-drop need 940x184. Viewing needs 152x108. --audio adds 44 points of height and requires at least 320 points of width. --no-clipboard removes only the clipboard strip.",
            2,
        ));
    }
    let refused = if connection.capture_system_shortcuts {
        Some("--capture-system-shortcuts is not implemented on macOS yet.")
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
            "Native robot input is not implemented on macOS. A foreground view may explicitly publish read-only observation metadata.",
        ));
    }
    connect_profile(runtime, cx, shutdown, stopped, api, connection, json, None)
}

#[allow(clippy::too_many_arguments)]
fn connect_profile(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    connection: &Connection,
    json: bool,
    publication: Option<&Publication>,
) -> Result<String, Failure> {
    let prepared = (|| {
        check(connection)?;
        let audio = playback::prepare(connection.audio.as_ref())?;
        let microphone = microphone::prepare(connection.microphone.as_ref())?;
        // Pin each native choice immediately after its original panel closes.
        // A second chooser can wait for two minutes; no unpinned pathname from
        // the first selection may wait behind that panel.
        let chosen_directory = if connection.choose_receive_directory {
            Some(file_choice::select_directory(
                runtime, cx, shutdown, stopped,
            )?)
        } else {
            None
        };
        let download = files::receive(connection, chosen_directory.as_deref())?;
        drop(chosen_directory);
        let chosen = if connection.upload.choose_files {
            Some(file_choice::select(runtime, cx, shutdown, stopped)?)
        } else {
            None
        };
        let selected_files = files::select(connection, chosen.as_deref(), download)?;
        Ok((selected_files, audio, microphone))
    })()
    .map_err(|mut error| {
        // No receive worker exists in this preflight scope. Cancellation,
        // invalid destinations and local device failures are known non-starts.
        files::preserve_not_started(connection, &mut error);
        error
    })?;
    let (selected_files, audio, microphone) = prepared;
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
        audio.as_ref(),
        microphone,
        publication,
    )
    .map_err(|mut error| {
        pending.preserve(&mut error);
        clipboard::preserve(&pending_clipboard, &mut error);
        let audio = audio.as_ref().map(|report| report.borrow());
        playback::preserve(audio.as_deref(), &mut error);
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
    audio: Option<&playback::Report>,
    microphone: Option<microphone::Prepared>,
    publication: Option<&Publication>,
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
    // `--pick-only` (fr app): the selection report is the whole result; the
    // application starts the connection as a separate command.
    if connection.pick.is_some() {
        return Ok(String::new());
    }
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
    // The screen's 1:1 box below the title bar, the 44-point sharing strip
    // and explicitly selected text/audio controls, before the host sizes video.
    let reserved =
        44 + if connection.control { 76 } else { 0 } + if audio.is_some() { 44 } else { 0 };
    let picture_box = fr_native::macos::window_box(reserved);
    let mut session = Session::new(
        connection.control.then(ControlPolicy::default),
        ObserverPolicy::default(),
        choose,
        epoch,
        format!("FrankenRemote: {}", selected.node),
    )
    .with_fit_window(connection.fit_window)
    .with_picture_box(picture_box)
    .with_files(selected_files)
    .clipboard(connection.control && connection.clipboard);
    session = playback::install(session, audio);
    session = microphone::install(session, microphone);
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
        // Pixels beyond what the window shows would be captured, encoded,
        // sent and decoded only to be scaled away; a host that can scale
        // sends the largest picture that fits, drawn 1:1. A resized window
        // states its new box when it reopens.
        picture: session.stated_picture(),
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
    if let Some(publication) = publication {
        owner = owner.with_observations(publication.publisher.clone());
        let sink = publication.clone();
        session = session
            .with_observations(move |event| sink.publish(event))
            .map_err(|_| {
                failure(
                    "observation_owner_unavailable",
                    "The original view could not install its observation publisher. No replacement connection was opened.",
                )
            })?;
    }
    session
        .file_command_connection(&owner.info().instance)
        .map_err(crate::file_commands::local_failure)?;
    let selector = selected.selector();
    let result = if connection.control {
        // Drop the Ref before handing the mutable session to its supervisor.
        // Clipboard and file consent are independent original choices.
        let offer = playback::offer(session.files().offer(connection.clipboard), audio.is_some())?;
        // The session's precise-scroll and text capabilities join the ORIGINAL
        // offer (playback reserved their room): each attempt may only remove.
        let offer = session.control_offer(offer).map_err(|_| {
            Failure::new(
                "control_capability_limit",
                "The selected Mac control profile cannot fit its precise-scroll and text capabilities. Use --no-clipboard or select fewer file directions.",
                2,
            )
        })?;
        let offer = microphone::offer(offer, connection.microphone.is_some())?;
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
            playback::offer(fr_client::native::observation_offer(), audio.is_some())?,
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
    let report = session.report();
    let evidence = files::Summary::capture(&session.files());
    let result = evidence.finish(
        finish(
            &report,
            result,
            stopped.get(),
            json,
            session.cleanup_confirmed(),
            reconnect::Application::terminal_failure(&session),
            session.picker_error(),
        ),
        json,
    );
    let audio = audio.map(|report| report.borrow());
    let outcome = playback::finish(audio.as_deref(), result, json);
    if let Some(diag_path) = connection.diagnostics_out.as_deref()
        && let Err(export_err) = crate::diagnostics::export_session(
            diag_path,
            report.media.as_ref(),
            fr_client::diagnostics::CodecPathKind::HardwareVideoToolbox,
            fr_client::diagnostics::TransportKind::NativeQuic,
            outcome.as_ref().err().map(|f| f.code),
        )
        && outcome.is_ok()
    {
        return Err(export_err);
    }
    outcome
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
        microphone::preserve(report.microphone.as_ref(), &mut error);
        error
    });
    media::finish(
        report,
        clipboard::finish(
            &report.clipboard,
            history::finish(report, result, json),
            json,
        ),
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
                    "The viewer's geometry changed and a checked fresh viewing attempt could not complete. The original input mapping and control ended; reconnect, then request control again with F8. Nothing is replayed.",
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
    let microphone = microphone::summary(report.microphone.as_ref(), json);
    let mut out = String::new();
    if json {
        let _ = writeln!(
            out,
            "{{\"outcome\":\"success\",\"platform\":\"macos\",\"attempts\":{},\"viewing_reconnects\":{},\"opened\":{},\"presented\":{},\"hardware_decode\":{},\"approval_requested\":{},\"control\":{{\"requested\":{},\"requests\":{},\"granted\":{},\"keys\":{},\"pointer\":{},\"buttons\":{},\"scrolls\":{},\"results\":{}}},\"ended\":\"{}\",\"local_cleanup_confirmed\":true,\"close_exchange\":{},\"lease_revocation\":{}{microphone}}}",
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
        if !microphone.is_empty() {
            let _ = writeln!(out, "{microphone}");
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
            "--choose-receive-directory",
            "--receive /private/inbox",
            "--send /private/chosen --receive /private/inbox",
        ] {
            let connection = connection(flags);
            assert!(check(&connection).is_ok());
            assert!(!connection.upload.file_drop && !connection.upload.file_commands);
        }
        for flags in [
            "--file-drop",
            "--file-drop --send /private/chosen",
            "--file-drop --file-commands",
            "--file-drop --file-commands --send /private/chosen --receive /private/inbox",
        ] {
            let connection = connection(flags);
            assert!(check(&connection).is_ok());
            assert!(connection.upload.file_drop);
        }
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
        let mut chosen = connection("--choose-receive-directory");
        chosen.control = false;
        assert_eq!(
            check(&chosen).err().unwrap().code,
            "receive_requires_control"
        );
        chosen.control = true;
        chosen.receive = connection("--receive /private/inbox").receive;
        assert_eq!(
            check(&chosen).err().unwrap().code,
            "receive_directory_choice_invalid"
        );
        let mut commands = connection("--file-commands");
        commands.control = false;
        assert_eq!(
            check(&commands).err().unwrap().code,
            "file_commands_require_control"
        );
        let mut native = connection("--file-drop");
        native.control = false;
        assert_eq!(
            check(&native).err().unwrap().code,
            "file_drop_requires_control"
        );
        for (flags, width, height) in [
            ("", 479, 184),
            ("", 480, 183),
            ("--send /private/chosen", 565, 184),
            ("--file-commands", 565, 184),
            ("--receive /private/inbox", 566, 183),
            ("--choose-receive-directory", 565, 184),
            ("--send /private/chosen --receive /private/inbox", 795, 184),
            ("--file-commands --receive /private/inbox", 795, 184),
            ("--file-drop", 709, 184),
            (
                "--file-drop --file-commands --receive /private/inbox",
                939,
                184,
            ),
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
            ("--file-drop", 710),
            ("--file-drop --file-commands --receive /private/inbox", 940),
            ("--file-drop --no-clipboard", 526),
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
