//! `fr-windows view`: one session in a native window. The window asks the
//! host for a picture no larger than its client area (physical pixels,
//! per-monitor DPI aware), so the host's pixels land 1:1 on the screen; the
//! newest decoded picture is converted and presented on the next vertical
//! blank. With `--control`, pointer, buttons, wheel and physical keys go to
//! the host while it grants control.
//!
//! Minimizing hides the view, which (as on iOS) ends a controlled session:
//! frd's contract ends the session with control. Losing focus releases
//! every key and button this window holds.
//!
//! A resize that would change the picture (a finished drag, maximize or
//! restore) can reopen an observing session stating the new client size:
//! the session core states its box once, and the host fits a
//! fresh source to each new viewer. Only a host seen scaling gets a reopen:
//! one that sends native pixels would send the same picture again after
//! seconds without one. Reopen requires successful retirement and the
//! original requested close; it resumes viewing only. Native requested close
//! fences input while retaining the original owner through its terminal
//! exchange. A failed cleanup still terminates instead. A reopened control-capable view
//! exposes a fresh Input → Request control choice, never replay of the earlier
//! --control choice.
//!
//! Leaving the active controlling window fences that original grant and closes
//! it in order. After positive bounded cleanup the window can wait dormant for
//! a new foreground event, then open fresh viewing without input or auxiliary
//! authority. Hidden/failure reports remain distinct from cleanup proof.
mod control;
mod cursor;
mod display;
mod text;
mod upload;

use super::render::Renderer;
use crate::{
    change_display::{self, Action, Activation, Cause, Refusal, Replacements},
    geometry::{Mapping, Rect as PictureRect, StaleMapping, fit, host_scales, reopen_for},
    keys,
};
use fr_mobile_ffi::{
    events::{Event, kind, state},
    session::{
        CapturedInput, CapturedKey, ClipboardReport, Config, InputResult, Session, Shared, end,
    },
    video,
};
use std::{
    cell::{Cell, RefCell},
    fmt::Write as _,
    io::Write as _,
    process::ExitCode,
    sync::Arc,
    time::{Duration, Instant},
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{ScreenToClient, ValidateRect};
use windows::Win32::Media::MediaFoundation::IMFSample;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetForegroundWindow, GetMessageTime, HTCLIENT, IDC_ARROW, IsDialogMessageW, IsIconic,
    LoadCursorW, MSG, MsgWaitForMultipleObjects, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
    PostQuitMessage, QS_ALLINPUT, RegisterClassExW, SIZE_MAXIMIZED, SIZE_MINIMIZED, SIZE_RESTORED,
    SPI_GETWORKAREA, SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ShowWindow,
    SystemParametersInfoW, TranslateMessage, WINDOW_EX_STYLE, WM_ACTIVATEAPP, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_ENTERSIZEMOVE, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_INITMENUPOPUP, WM_KEYDOWN,
    WM_KEYFIRST, WM_KEYLAST, WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_MOVE, WM_PAINT, WM_QUIT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, Interface, w};

/// The window's session, renderer and input state, owned by the UI thread.
struct Viewer {
    session: Session,
    renderer: Renderer,
    /// The last drawn picture's size and place (input maps through these).
    picture: (u32, u32),
    mapping: Option<Mapping>,
    /// Held buttons (`session.button` indices) and the last pointer point.
    buttons: [bool; 5],
    last_point: (f64, f64),
    /// Key transitions waiting behind a press the session answered `Busy`.
    keys: keys::Deferred<CapturedKey>,
    /// The host's display size (its first VIDEO event).
    display: (u32, u32),
    /// Inside a drag's size/move loop: reopen only once it ends.
    sizing: bool,
    /// A reopen the current session is closing for: the next session
    /// states the client size current when this one finishes.
    reopen: Replacements,
    /// A requested replacement that could not prove safe original cleanup.
    reopen_refusal: Option<Refusal>,
    /// Closing for good (the user or `--seconds`): no more reopens.
    closing: bool,
    /// The box the current session stated (its client size at start).
    stated: (u32, u32),
    /// Whether this host fits pictures into the stated box, once a picture
    /// has shown it (`geometry::host_scales`).
    scales: Option<bool>,
    /// When the pending reopen was asked for (its gap is reported).
    reopen_asked: Option<Instant>,
    /// One local composition, permanently bound to its opening source/view.
    text: Option<text::Editor>,
    /// One modeless choice from this attempt's host-approved catalog.
    display_picker: Option<display::Picker>,
    /// Visible ownership state and an original one-use local menu choice.
    input_menu: control::Menu,
    /// Original selected file and its evidence survive every viewing replacement.
    upload_menu: Option<upload::Menu>,
    /// First parent-close cleanup sample for the original native file chooser.
    /// Neither repeated close callbacks nor later session completion renew it.
    file_cleanup_started: Option<u64>,
    /// Cursor pixels an accepted or uncertain submission may have left in
    /// the swap chain, scoped to their original connection.
    cursor_drawn: Option<(u64, PictureRect)>,
    /// Accepted picture geometry for the native cursor, never a pending frame.
    picture_drawn: Option<(u64, u64, PictureRect)>,
    /// A static picture must be retried after a skipped Present or foreground
    /// transition, retaining its original frame and color-range identity.
    repaint_pending: bool,
    local_cursor: cursor::Local,
}
thread_local! {
    static VIEWER: RefCell<Option<Viewer>> = const { RefCell::new(None) };
    // A nested native callback may find VIEWER borrowed. Closure remains due
    // before the pump processes another input event or replaces the session.
    static CLOSE_PENDING: Cell<bool> = const { Cell::new(false) };
    // Activation messages describe a transition. Restore visible observation
    // only after the pump sees this original viewer actually in foreground.
    static ACTIVATION_PENDING: Cell<Option<Activation>> = const { Cell::new(None) };
    /// The original owner was already fenced by the independent menu handler.
    /// UI cleanup may wait until a nested native callback releases Viewer.
    static CHANGE_PENDING: RefCell<Option<(Cause, change_display::Request<Shared>)>> =
        const { RefCell::new(None) };
}
/// Runs `f` on the viewer unless it is absent or already borrowed (a
/// message dispatched from inside another viewer call).
fn with_viewer<R>(f: impl FnOnce(&mut Viewer) -> R) -> Option<R> {
    VIEWER.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

fn drain_close() {
    if CLOSE_PENDING.with(Cell::get) {
        CHANGE_PENDING.with(|pending| *pending.borrow_mut() = None);
        with_viewer(|viewer| {
            // Clear before native cleanup, which can itself reenter WM_CLOSE.
            CLOSE_PENDING.with(|pending| pending.set(false));
            viewer.close();
        });
        return;
    }
    let mut requested = CHANGE_PENDING.with(|pending| pending.borrow_mut().take());
    if requested.is_some() {
        with_viewer(|viewer| {
            if let Some((cause, request)) = requested.take() {
                viewer.replace_requested(cause, &request);
            }
        });
        // A native callback can enter while Viewer is borrowed. Preserve the
        // single original request; the next pump turn consumes it before input.
        if requested.is_some() {
            CHANGE_PENDING.with(|pending| *pending.borrow_mut() = requested);
        }
    }
}

#[derive(Default)]
struct Stats {
    /// Sessions reopened for a new picture size.
    reopens: u64,
    display_changes: u64,
    control_releases: u64,
    focus_releases: u64,
    /// Actual last retired focus owner's public result, separate from the
    /// current viewing owner's end. Bounded to one content-free result.
    focus_end: Option<(u32, Option<String>)>,
    /// Counts of the sessions before the current one.
    earlier_decoded: u64,
    earlier_decode_micros: u64,
    earlier_presented: u64,
    /// The original configured clipboard's evidence survives a viewing resize.
    clipboard: Option<ClipboardReport>,
    /// The first session's time to its first view.
    first_view_micros: Option<u64>,
    /// Longest wait from a reopen's request to its first drawn picture.
    reopen_gap_micros: u64,
    drawn: u64,
    /// Cursor/placement redraws of the same original frame, never new video.
    repaints: u64,
    draw_failures: u64,
    last_draw: Option<Instant>,
    /// Fixed-memory timing summary; no per-frame history survives a draw.
    intervals: fr_client::cadence::FrameCadence,
    picture: (u32, u32),
    display: (u32, u32),
}

pub fn run(mut config: Config, title: &str, seconds: Option<u64>) -> ExitCode {
    CLOSE_PENDING.with(|pending| pending.set(false));
    CHANGE_PENDING.with(|pending| *pending.borrow_mut() = None);
    // SAFETY: process-wide DPI awareness, set before any window exists, so
    // client-area sizes and mouse points are physical pixels.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let Some(gpu) = video::gpu() else {
        return refuse("no_d3d11_device");
    };
    let Ok(hwnd) = create_window(title) else {
        return refuse("window_unavailable");
    };
    let Ok(input_menu) = control::Menu::install(hwnd, title) else {
        // SAFETY: our UI-thread window, before it owns a session or resources.
        let _ = unsafe { DestroyWindow(hwnd) };
        return refuse("input_menu_unavailable");
    };
    let upload_menu = if let Some(original) = &config.upload {
        if let Ok(menu) = upload::Menu::install(hwnd, Arc::clone(original)) {
            Some(menu)
        } else {
            // SAFETY: our window, before any remote session starts.
            let _ = unsafe { DestroyWindow(hwnd) };
            return refuse("file_menu_unavailable");
        }
    } else {
        None
    };
    let size = client_size(hwnd);
    config.picture = Some(size);
    let control = config.control;
    let Ok(session) = Session::start_with_local_cursor(config.clone()) else {
        return refuse("session_thread_unavailable");
    };
    let Ok(renderer) = Renderer::new(&gpu.device, hwnd, size) else {
        return refuse("swap_chain_unavailable");
    };
    control::bind(&session.shared_handle());
    VIEWER.with(|cell| {
        *cell.borrow_mut() = Some(Viewer {
            session,
            renderer,
            picture: (0, 0),
            mapping: None,
            buttons: [false; 5],
            last_point: (0.0, 0.0),
            keys: keys::Deferred::default(),
            display: (0, 0),
            sizing: false,
            reopen: Replacements::default(),
            reopen_refusal: None,
            closing: false,
            stated: size,
            scales: None,
            reopen_asked: None,
            text: None,
            display_picker: None,
            input_menu,
            upload_menu,
            file_cleanup_started: None,
            cursor_drawn: None,
            picture_drawn: None,
            repaint_pending: false,
            local_cursor: cursor::Local::default(),
        });
    });
    // SAFETY: showing the window this thread created.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    control::visible(true);
    with_viewer(|v| {
        // Only the original invocation uses --control automatically. Native
        // window creation and its initial size events have already settled.
        if control {
            v.session.shared().request_control();
        }
        v.input_menu.refresh(v.session.shared());
    });
    let started = Instant::now();
    let stats = pump(seconds, hwnd, &config);
    let mut viewer = VIEWER.with(|cell| cell.borrow_mut().take());
    if let Some(viewer) = &mut viewer {
        cursor::retire(viewer, hwnd);
        viewer.text = None;
        viewer.display_picker = None;
    }
    // SAFETY: destroying the window this thread created; the viewer (and
    // with it the swap chain) is already out of the window procedure's
    // reach.
    let _ = unsafe { DestroyWindow(hwnd) };
    let Some(viewer) = viewer else {
        return refuse("viewer_lost");
    };
    report(&viewer, &stats, started.elapsed())
}

fn refuse(reason: &str) -> ExitCode {
    let _ = writeln!(
        std::io::stdout().lock(),
        "{{\"status\":\"refused\",\"reason\":\"{reason}\"}}"
    );
    ExitCode::from(1)
}

/// The message pump and draw loop, until the session ends (a session
/// closed for a reopen is replaced, not the end).
fn pump(seconds: Option<u64>, hwnd: HWND, config: &Config) -> Stats {
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs(s));
    let mut stats = Stats::default();
    let mut last_sequence = 0;
    let mut closing = false;
    let mut msg = MSG::default();
    loop {
        loop {
            drain_close();
            // SAFETY: the standard message pump on this thread's queue.
            if !unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                break;
            }
            // PeekMessage can dispatch sent callbacks before yielding this
            // queued event. An earlier close must beat any next remote input.
            drain_close();
            if msg.message == WM_QUIT {
                return stats;
            }
            // Native EDIT owns composition and physical typing. Dialog routing
            // happens outside the viewer borrow so Send/Cancel can borrow the
            // original owner synchronously instead of dropping the callback.
            let editor = with_viewer(|v| v.text.as_ref().map(text::Editor::window)).flatten();
            if let Some(editor) = editor {
                // SAFETY: our UI thread's live modeless editor and queued message.
                if unsafe { IsDialogMessageW(editor, &raw const msg) }.as_bool() {
                    continue;
                }
            }
            let picker =
                with_viewer(|v| v.display_picker.as_ref().map(display::Picker::window)).flatten();
            if let Some(picker) = picker {
                // SAFETY: our live modeless picker, outside the Viewer borrow;
                // keyboard/button callbacks keep the original request identity.
                if unsafe { IsDialogMessageW(picker, &raw const msg) }.as_bool() {
                    continue;
                }
            }
            // SAFETY: a message this thread just removed from its queue.
            unsafe {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        // A sent close may also arrive during a queue read that returns empty.
        drain_close();
        let Some((finished, drew)) = with_viewer(|v| step(v, &mut stats, &mut last_sequence, hwnd))
        else {
            return stats;
        };
        // Drawing may reenter a native close while step owns the viewer.
        drain_close();
        // The invocation's own deadline wins before a finished owner can be
        // replaced. A late timer must not create and then cancel a new dial.
        if !closing && deadline.is_some_and(|d| Instant::now() >= d) {
            closing = true;
            with_viewer(Viewer::close);
        }
        let replacing = with_viewer(|v| v.reopen.pending()).unwrap_or(false);
        if finished || replacing {
            match with_viewer(|v| reopen(v, &mut stats, hwnd, config)) {
                Some(Reopen::Started) => {
                    last_sequence = 0;
                    continue;
                }
                Some(Reopen::Waiting) => {}
                Some(Reopen::No) => {
                    if !with_viewer(Viewer::pending_file_cleanup).unwrap_or(false) {
                        return stats;
                    }
                }
                None => return stats,
            }
        }
        if !drew {
            let wait = if with_viewer(|v| v.reopen.dormant()).unwrap_or(false) {
                50
            } else {
                2
            };
            // SAFETY: waits for input or a bounded timeout; no handles. A
            // disconnected dormant window does not need the presentation tick.
            unsafe { MsgWaitForMultipleObjects(None, false, wait, QS_ALLINPUT) };
        }
    }
}

/// Replaces only the original positively retired owner, after the bounded
/// listener cooldown. A manual switch opens a fresh approved-catalog picker;
/// resize keeps that policy after the first explicit switch.
#[allow(clippy::too_many_lines, reason = "one ordered replacement sequence")]
fn reopen(v: &mut Viewer, stats: &mut Stats, hwnd: HWND, config: &Config) -> Reopen {
    let finished = v.session.shared().finished();
    let no_replacement = || {
        if finished {
            Reopen::No
        } else {
            Reopen::Waiting
        }
    };
    if v.closing {
        return no_replacement();
    }
    let lifetime = control::lifetime();
    let focus = v.reopen.focus_pending();
    let cleaned = if focus {
        v.session.shared().focus_closed()
    } else {
        v.session.shared().orderly_closed()
    };
    let opening = match v.reopen.poll(
        input_ticks(),
        lifetime,
        control::visible_now(),
        finished
            && v.upload_menu
                .as_ref()
                .is_none_or(upload::Menu::cleanup_complete),
        cleaned,
    ) {
        Action::Open(opening) => opening,
        Action::Waiting => return Reopen::Waiting,
        Action::Dormant => {
            v.input_menu.waiting_for_foreground();
            return Reopen::Waiting;
        }
        Action::None => return no_replacement(),
        Action::Refused(reason) => {
            eprintln!("fr-windows: {}", reason.name());
            v.reopen_refusal.get_or_insert(reason);
            v.close();
            return no_replacement();
        }
    };
    v.text = None;
    v.display_picker = None;
    cursor::retire(v, hwnd);
    if v.closing {
        return Reopen::No;
    }
    let mut next = config.clone();
    // A replacement view cannot resend the original explicit file. Its menu
    // retains only the original transfer's cancellation and receipt evidence.
    next.upload = None;
    if matches!(opening.cause, Cause::StopControl | Cause::FocusLost) {
        next.clipboard = false;
    }
    if opening.choose_display {
        next.display = fr_mobile_ffi::session::DisplayChoice::Choose;
    }
    let clipboard = v.session.shared().clipboard_report();
    if clipboard.configured || clipboard.reason.is_some() {
        stats.clipboard.get_or_insert(clipboard);
    }
    if stats.clipboard.is_some() {
        // A replacement view cannot restart the command's retired clipboard,
        // including after its independent privacy window's Turn off action.
        next.clipboard = false;
    }
    // Retain only the offered profile. Session::start grants and requests
    // nothing: a resized view needs a new explicit local menu choice.
    let size = client_size(hwnd);
    // SAFETY: scalar queries of this UI thread's original live viewer. A
    // foreground callback never authorizes opening while it is hidden again.
    let foreground = unsafe { GetForegroundWindow() == hwnd && !IsIconic(hwnd).as_bool() };
    if CLOSE_PENDING.with(Cell::get)
        || !control::visible_now()
        || control::lifetime() != lifetime
        || (opening.cause == Cause::FocusLost && !foreground)
    {
        v.reopen_refusal.get_or_insert(Refusal::Cancelled);
        v.close();
        return Reopen::No;
    }
    next.picture = Some(size);
    // Native window queries and the original clipboard report can wait. The
    // ready opening carries the same absolute bound through both of them.
    if let Err(reason) = opening.check(input_ticks()) {
        v.reopen_refusal.get_or_insert(reason);
        v.close();
        return Reopen::No;
    }
    let Ok(session) = Session::start_with_local_cursor(next) else {
        v.reopen_refusal.get_or_insert(Refusal::Start);
        v.close();
        return Reopen::No;
    };
    v.stated = size;
    let old = std::mem::replace(&mut v.session, session);
    control::bind(&v.session.shared_handle());
    let shared = old.shared();
    let (decoded, decode_micros) = shared.frames.decode_stats();
    stats.earlier_decoded += decoded;
    stats.earlier_decode_micros += decode_micros;
    stats.earlier_presented += shared.frames.presented();
    stats
        .first_view_micros
        .get_or_insert(shared.first_view_micros());
    stats.reopens += 1;
    stats.display_changes += u64::from(opening.cause == Cause::Display);
    stats.control_releases += u64::from(opening.cause == Cause::StopControl);
    if opening.cause == Cause::FocusLost {
        stats.focus_releases += 1;
        stats.focus_end = Some((shared.end_code(), shared.detail()));
    }
    // The reopen's own wait is not a frame interval.
    stats.last_draw = None;
    v.mapping = None;
    v.picture = (0, 0);
    v.picture_drawn = None;
    v.repaint_pending = false;
    v.display = (0, 0);
    v.scales = None;
    v.buttons = [false; 5];
    v.keys = keys::Deferred::default();
    control::visible(true);
    v.input_menu.refresh(v.session.shared());
    Reopen::Started
}

enum Reopen {
    Started,
    /// The finished session's host is still rebinding: dial a little later.
    Waiting,
    No,
}

/// Collect a finished file-picker worker and refresh the upload menu.
fn poll_file_choice(v: &mut Viewer, hwnd: HWND) {
    // Remember the pending worker before polling can collect it. Collection
    // after the original cleanup bound must remain a failure, including when
    // the source/transfer checks inside poll waited across that bound.
    let collecting_file_cleanup = v.file_cleanup_started.is_some() && v.choosing_file();
    if collecting_file_cleanup {
        v.file_cleanup_expired();
    }
    let file_available = v.can_choose_file();
    if let Some(menu) = &mut v.upload_menu {
        menu.poll(
            &v.session.shared_handle(),
            control::lifetime(),
            file_available,
        );
    }
    // The worker may have fenced its original grant while poll collected it.
    // Its independent pending notification survives that collection.
    sync_file_focus(v, hwnd);
    if collecting_file_cleanup {
        v.file_cleanup_expired();
    }
    if let Some(menu) = &mut v.upload_menu {
        menu.refresh();
    }
}

/// Consume one current foreground observation. A foreground clear can reenter
/// Win32, so another hide/resize/close during that work still cancels the choice.
fn apply_activation(v: &mut Viewer, hwnd: HWND) {
    let Some(activation) = ACTIVATION_PENDING.with(Cell::get) else {
        return;
    };
    if v.closing
        || (v.reopen.pending() && !v.reopen.focus_pending())
        || !activation.current(control::activation_scope(), input_ticks())
    {
        ACTIVATION_PENDING.with(|pending| pending.set(None));
        return;
    }
    // SAFETY: scalar queries against this UI thread's live viewer.
    if unsafe { GetForegroundWindow() != hwnd || IsIconic(hwnd).as_bool() } {
        return;
    }
    ACTIVATION_PENDING.with(|pending| pending.set(None));
    let focus = v.reopen.focus_pending();
    if focus && let Err(reason) = v.reopen.resume_focus(activation, input_ticks()) {
        v.reopen_refusal.get_or_insert(reason);
        v.close();
        return;
    }
    // The retired owner remains permanently closed. Reflect this admitted
    // foreground event before native work so a nested hide changes its scope.
    control::visible(true);
    v.picture_drawn = None;
    v.repaint_pending = true;
    if !focus {
        return;
    }
    // The earlier background Present may have been occluded. Clear again now
    // the window is foreground, without marking any remote frame as presented.
    if v.renderer.clear().is_err() {
        v.session.shared().presentation_failed();
        v.reopen_refusal.get_or_insert(Refusal::Presentation);
        v.close();
        return;
    }
    if v.renderer.present_accepted() {
        v.cursor_drawn = None;
    }
    // SAFETY: native rendering above may have dispatched lifecycle callbacks;
    // recheck the same live viewer after it and retain the opening's old bound.
    let foreground = unsafe { GetForegroundWindow() == hwnd && !IsIconic(hwnd).as_bool() };
    if CLOSE_PENDING.with(Cell::get)
        || control::activation_scope() != Some((activation.lifetime, activation.attempt))
        || !foreground
    {
        v.reopen_refusal.get_or_insert(Refusal::Cancelled);
        v.close();
    }
}

/// Events, then the newest picture if there is one. (finished, drew)
fn step(v: &mut Viewer, stats: &mut Stats, last_sequence: &mut u64, hwnd: HWND) -> (bool, bool) {
    sync_file_focus(v, hwnd);
    apply_activation(v, hwnd);
    // Level-triggered, so no lost or rescoped activation message can leave a
    // paused owner gated: input resumes once this window is actually in front.
    if !v.closing && (!v.session.shared().active() || v.picture_drawn.is_none()) {
        // SAFETY: scalar queries against this UI thread's live viewer.
        let foreground = unsafe { GetForegroundWindow() == hwnd && !IsIconic(hwnd).as_bool() };
        if foreground {
            if !v.session.shared().active() {
                let _ = control::active(true);
                v.picture_drawn = None;
            }
            // An owned text/display window can take foreground without an
            // app switch. Its return still needs a retained-picture repaint,
            // even when no video or cursor update arrived while it was open.
            if v.session.shared().active() && v.picture_drawn.is_none() {
                v.repaint_pending = true;
            }
        }
    }
    poll_file_choice(v, hwnd);
    let can_change_display = v.can_change_display();
    v.input_menu.display_available(can_change_display);
    v.input_menu.refresh(v.session.shared());
    v.update_display_picker(hwnd);
    if !v.keys.is_empty() {
        v.send_keys();
    }
    if v.text.as_ref().is_some_and(text::Editor::dismissed) {
        v.text = None;
    }
    if let Some(editor) = &mut v.text {
        editor.refresh();
    }
    let shared = v.session.shared();
    while let Some(event) = shared.events.poll() {
        log_event(&event);
        // One VIDEO event names the host display, a later one the encoded
        // picture; each leaves the other zero.
        if event.kind == kind::VIDEO && event.display_width != 0 {
            stats.display = (event.display_width, event.display_height);
            v.display = stats.display;
        }
    }
    if shared.finished() {
        cursor::retire(v, hwnd);
        return (true, false);
    }
    // Once the original owner is fenced, no old picture can satisfy a new
    // view or consume the pending replacement's time-to-first-picture metric.
    if v.reopen.pending() || v.closing {
        cursor::retire(v, hwnd);
        return (false, false);
    }
    let Ok(cursor_changed) = cursor::synchronize(v, hwnd) else {
        eprintln!("fr-windows: cursor_presentation_failed");
        v.session.shared().presentation_failed();
        v.close();
        cursor::retire(v, hwnd);
        stats.draw_failures += 1;
        return (false, false);
    };
    cursor::refresh(v, hwnd);
    draw_latest(v, stats, last_sequence, hwnd, cursor_changed)
}

/// Redraw the newest retained picture when either video or cursor changed.
/// Cursor-only redraws keep the original picture's identity and evidence.
#[allow(
    clippy::too_many_lines,
    reason = "one ordered draw and recheck sequence"
)]
fn draw_latest(
    v: &mut Viewer,
    stats: &mut Stats,
    last_sequence: &mut u64,
    hwnd: HWND,
    cursor_changed: bool,
) -> (bool, bool) {
    let shared = v.session.shared();
    // A local resize/move invalidates its coordinate mapping immediately.
    // Redraw the one retained picture even on a static desktop, preserving
    // that picture's ORIGINAL frame id; this creates no new source evidence.
    let after = if v.mapping.is_some() && !cursor_changed && !v.repaint_pending {
        *last_sequence
    } else {
        0
    };
    let Some((raw, info)) = shared.frames.take_newer(after) else {
        return (false, false);
    };
    // SAFETY: take ownership of the slot's +1 reference BEFORE every early
    // return, including a connection change that overtook this snapshot.
    let sample = unsafe { IMFSample::from_raw(raw) };
    // Select the picture before the final cursor snapshot. A decoder reset
    // clears its cursor before publishing new pictures; the reverse order
    // could put the preceding decoder's cursor on this newer picture.
    if cursor::synchronize(v, hwnd).is_err() {
        eprintln!("fr-windows: cursor_presentation_failed");
        v.session.shared().presentation_failed();
        v.close();
        cursor::retire(v, hwnd);
        stats.draw_failures += 1;
        return (false, false);
    }
    let shared = v.session.shared();
    let attempt = shared.frames.app_id(0);
    if attempt == 0 || shared.frames.app_id(info.frame) != info.frame {
        cursor::retire(v, hwnd);
        return (false, false);
    }
    let picture = (info.width, info.height);
    let overlay = v
        .renderer
        .cursor_visible(picture, attempt, info.sequence)
        .then(|| fit(picture, v.renderer.size()))
        .flatten()
        .map(|dest| (attempt, dest));
    if cursor::before_draw(v, hwnd, overlay).is_err() {
        stats.draw_failures += 1;
        v.session.shared().presentation_failed();
        v.close();
        cursor::retire(v, hwnd);
        return (false, false);
    }
    let lifetime = control::lifetime();
    let drawn = video::texture_of(&sample).and_then(|(texture, slice)| {
        v.renderer
            .draw(
                &texture,
                slice,
                picture,
                info.full_range,
                attempt,
                info.sequence,
            )
            .ok()
            .flatten()
    });
    let Some(dest) = drawn else {
        // A failed Present has an unknown display outcome. Preserve evidence
        // of either old or newly submitted cursor pixels until cleanup.
        v.cursor_drawn = overlay.or(v.cursor_drawn);
        v.picture_drawn = None;
        stats.draw_failures += 1;
        v.session.shared().presentation_failed();
        v.close();
        cursor::retire(v, hwnd);
        return (false, false);
    };
    if !v.renderer.present_accepted() {
        v.cursor_drawn = overlay.or(v.cursor_drawn);
        v.picture_drawn = None;
        v.repaint_pending = true;
        return (false, false);
    }
    // Record the just-submitted image BEFORE checking callbacks which could
    // have retired its owner during native work. Cleanup must erase it too.
    v.cursor_drawn = overlay;
    v.picture_drawn = Some((attempt, info.sequence, dest));
    let shared = v.session.shared();
    if v.closing
        || v.reopen.pending()
        || shared.finished()
        || shared.frames.app_id(0) != attempt
        || control::lifetime() != lifetime
    {
        v.mapping = None;
        cursor::retire(v, hwnd);
        return (false, false);
    }
    // SetCursor's ownership is local to this thread/window. A background
    // submission cannot supply native shape geometry on a later activation.
    // SAFETY: scalar queries of this UI thread's original live viewer.
    if !shared.active() || unsafe { GetForegroundWindow() != hwnd || IsIconic(hwnd).as_bool() } {
        v.picture_drawn = None;
    }
    v.repaint_pending = false;
    let shared = v.session.shared();
    v.picture = (info.width, info.height);
    if v.mapping
        .as_ref()
        .is_none_or(|mapping| !mapping.same_layout(dest, v.picture))
    {
        v.mapping = Some(Mapping::new(dest, v.picture, input_ticks()));
        shared.scroll_end();
    }
    if v.scales.is_none() {
        v.scales = host_scales(v.display, v.stated, v.picture);
    }
    shared.frames.mark_drawn(info.frame);
    let new_picture = info.sequence > *last_sequence;
    *last_sequence = info.sequence;
    if !new_picture {
        stats.repaints += 1;
        cursor::refresh(v, hwnd);
        return (false, true);
    }
    let now = Instant::now();
    if let Some(asked) = v.reopen_asked.take() {
        let gap = u64::try_from(now.duration_since(asked).as_micros()).unwrap_or(u64::MAX);
        stats.reopen_gap_micros = stats.reopen_gap_micros.max(gap);
    }
    if let Some(last) = stats.last_draw {
        stats
            .intervals
            .record(u64::try_from(now.duration_since(last).as_micros()).unwrap_or(u64::MAX));
    }
    stats.last_draw = Some(now);
    stats.drawn += 1;
    stats.picture = v.picture;
    cursor::refresh(v, hwnd);
    (false, true)
}

fn log_event(event: &Event) {
    let name = match (event.kind, event.value) {
        (kind::STATE, state::CONNECTING) => "connecting",
        (kind::STATE, state::AUTHENTICATED) => "authenticated",
        (kind::STATE, state::AWAITING_APPROVAL) => "awaiting_approval",
        (kind::STATE, state::VIEWING) => "viewing",
        (kind::STATE, state::REQUESTING_CONTROL) => "requesting_control",
        (kind::STATE, state::CONTROLLED) => "controlled",
        (kind::STATE, state::RECONNECTING) => "reconnecting",
        (kind::STATE, state::CLOSING) => "closing",
        (kind::STATE, state::CONTROL_ENDED) => "control_ended",
        (kind::VIDEO, _) => {
            eprintln!(
                "fr-windows: video display {}x{} picture {}x{} (attempt {})",
                event.display_width,
                event.display_height,
                event.picture_width,
                event.picture_height,
                event.attempt
            );
            return;
        }
        (kind::PIN, _) => "pin",
        (kind::ENDED, code) => {
            eprintln!("fr-windows: ended {}", end_name(code));
            return;
        }
        (kind::OVERFLOW, _) => "event_overflow",
        _ => "other",
    };
    eprintln!("fr-windows: {name} (attempt {})", event.attempt);
}

const fn end_name(code: u32) -> &'static str {
    match code {
        end::OK => "ok",
        end::HOST_CLOSED => "host_closed",
        end::NAME_UNRESOLVED => "name_unresolved",
        end::OUTSIDE_TAILNET => "outside_tailnet",
        end::IDENTITY_CHANGED => "identity_changed",
        end::TLS_REFUSED => "tls_refused",
        end::UNREACHABLE => "unreachable",
        end::HOST_REFUSED => "host_refused",
        end::SESSION_FAILED => "session_failed",
        end::CANCELLED => "cancelled",
        end::INTERNAL => "internal",
        end::CONTROL_STOPPED => "control_stopped",
        end::HOST_BUSY => "host_busy",
        _ => "unknown",
    }
}

/// The completion document (content-free counts and timings) on stdout.
/// Exit 0 only when pictures were drawn and the session ended in order.
fn report(viewer: &Viewer, stats: &Stats, elapsed: Duration) -> ExitCode {
    let shared = viewer.session.shared();
    let code = shared.end_code();
    let (decoded, decode_micros) = shared.frames.decode_stats();
    let (decoded, decode_micros) = (
        decoded + stats.earlier_decoded,
        decode_micros + stats.earlier_decode_micros,
    );
    // Quantiles are conservative upper bounds from 1 ms buckets. Above
    // 250 ms the overflow bucket reports the actual maximum, never a clipped
    // stall. Report construction neither clones nor sorts frame history.
    let interval_ms = serde_json::json!({
        "p50": stats.intervals.quantile_upper_ms(500),
        "p95": stats.intervals.quantile_upper_ms(950),
        "p99": stats.intervals.quantile_upper_ms(990),
        "max": stats.intervals.max_us().map(micros_to_ms),
        "percentile_kind": "upper_bound",
        "bucket_ms": 1,
        "samples": stats.intervals.gaps(),
    });
    let mean_decode = decode_micros.checked_div(decoded).map_or(0.0, micros_to_ms);
    let clipboard = stats.clipboard.unwrap_or_else(|| shared.clipboard_report());
    let clipboard = serde_json::json!({
        "requested": clipboard.requested,
        "configured": clipboard.configured,
        "native_started": clipboard.native_started,
        "received_submitted": clipboard.received_submitted,
        "received_unknown": clipboard.received_unknown,
        "received_refused": clipboard.received_refused,
        "cleanup": clipboard.cleanup_name(),
        "reason": clipboard.reason.map(|reason| format!("{reason:?}")),
    });
    let upload_ok = viewer
        .upload_menu
        .as_ref()
        .is_none_or(upload::Menu::succeeded);
    let upload = upload::document(viewer.upload_menu.as_ref());
    let focus_end = stats.focus_end.as_ref().map(|(code, detail)| {
        serde_json::json!({
            "end": end_name(*code),
            "detail": detail,
            "cleanup": "finished",
        })
    });
    let mut json = String::new();
    let _ = write!(
        json,
        "{{\"status\":\"ended\",\"end\":\"{}\",\"detail\":{},\"drawn\":{},\"repaints\":{},\"draw_failures\":{},\
         \"presented\":{},\"decoded\":{},\"decode_ms_mean\":{:.2},\"first_view_ms\":{:.1},\
         \"dxva\":{},\"display\":[{},{}],\"picture\":[{},{}],\"window\":[{},{}],\
         \"frame_interval_ms\":{},\
         \"reopens\":{},\"display_changes\":{},\"control_releases\":{},\"focus_releases\":{},\
         \"last_focus_end\":{},\"reopen_refusal\":{},\
         \"reopen_gap_ms\":{:.1},\"last_first_view_ms\":{:.1},\
         \"seconds\":{:.1},\"clipboard\":{},\"upload\":{}}}",
        end_name(code),
        shared
            .detail()
            .map_or_else(|| "null".to_owned(), |d| format!("{d:?}")),
        stats.drawn,
        stats.repaints,
        stats.draw_failures,
        shared.frames.presented() + stats.earlier_presented,
        decoded,
        mean_decode,
        micros_to_ms(
            stats
                .first_view_micros
                .unwrap_or_else(|| shared.first_view_micros())
        ),
        shared.hardware(),
        stats.display.0,
        stats.display.1,
        stats.picture.0,
        stats.picture.1,
        viewer.renderer.size().0,
        viewer.renderer.size().1,
        interval_ms,
        stats.reopens,
        stats.display_changes,
        stats.control_releases,
        stats.focus_releases,
        serde_json::to_string(&focus_end).unwrap_or_else(|_| "null".into()),
        viewer.reopen_refusal.map_or_else(
            || "null".to_owned(),
            |reason| format!("{:?}", reason.name()),
        ),
        micros_to_ms(stats.reopen_gap_micros),
        micros_to_ms(shared.first_view_micros()),
        elapsed.as_secs_f64(),
        clipboard,
        upload,
    );
    let _ = writeln!(std::io::stdout().lock(), "{json}");
    if stats.drawn > 0
        && matches!(code, end::OK | end::CANCELLED)
        && viewer.reopen_refusal.is_none()
        && upload_ok
    {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[allow(clippy::cast_precision_loss, reason = "timings far below 2^52 µs")]
fn micros_to_ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

fn create_window(title: &str) -> windows::core::Result<HWND> {
    // SAFETY: registers this module's window class (once per process; a
    // second registration fails harmlessly) and creates one window with
    // it on this thread.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("FrankenRemoteViewer");
        let wc = WNDCLASSEXW {
            cbSize: u32::try_from(size_of::<WNDCLASSEXW>()).unwrap_or(0),
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&raw const wc);
        let mut work = RECT::default();
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some((&raw mut work).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )?;
        let (width, height) = (
            (work.right - work.left) * 9 / 10,
            (work.bottom - work.top) * 9 / 10,
        );
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            &HSTRING::from(title),
            WS_OVERLAPPEDWINDOW,
            work.left + (work.right - work.left - width) / 2,
            work.top + (work.bottom - work.top - height) / 2,
            width,
            height,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn client_size(hwnd: HWND) -> (u32, u32) {
    let mut rect = RECT::default();
    // SAFETY: a live window and a writable rectangle.
    let _ = unsafe { GetClientRect(hwnd, &raw mut rect) };
    (
        u32::try_from(rect.right - rect.left).unwrap_or(0),
        u32::try_from(rect.bottom - rect.top).unwrap_or(0),
    )
}

/// The signed 16-bit LOWORD and HIWORD of a message parameter.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "masked to 16 bits first"
)]
const fn words(value: isize) -> (i16, i16) {
    (
        ((value & 0xffff) as u16).cast_signed(),
        (((value >> 16) & 0xffff) as u16).cast_signed(),
    )
}
/// Scan code (bits 16-23) and extended flag (bit 24) of a key message.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "bits 16-23 of lParam"
)]
const fn key_bits(lparam: LPARAM) -> (u8, bool) {
    (((lparam.0 >> 16) & 0xff) as u8, (lparam.0 >> 24) & 1 == 1)
}
/// The key message queued next for this window, left in the queue.
fn next_key(hwnd: HWND) -> Option<keys::KeyMessage> {
    let mut msg = MSG::default();
    // SAFETY: looks at (PM_NOREMOVE) this thread's next keyboard message
    // for its own window; nothing is dispatched or removed.
    let found = unsafe {
        PeekMessageW(
            &raw mut msg,
            Some(hwnd),
            WM_KEYFIRST,
            WM_KEYLAST,
            PM_NOREMOVE,
        )
    }
    .as_bool();
    let pressed = match msg.message {
        WM_KEYDOWN | WM_SYSKEYDOWN => true,
        WM_KEYUP | WM_SYSKEYUP => false,
        _ => return None,
    };
    let (scan, extended) = key_bits(msg.lParam);
    found.then_some(keys::KeyMessage {
        scan,
        extended,
        pressed,
        time: msg.time,
    })
}
fn point_of(lparam: LPARAM) -> (i32, i32) {
    let (x, y) = words(lparam.0);
    (i32::from(x), i32::from(y))
}

/// The same suspend-inclusive uptime domain as Win32 message creation time.
fn input_ticks() -> u64 {
    // SAFETY: reads an OS clock, with no handle, buffer or thread affinity.
    unsafe { GetTickCount64() }
}

/// Sample after the original source clock, so queue age also charges native
/// calls or lock waits between those samples. Shared by every message kind.
fn message_age(message: u32) -> Result<(u64, Duration), keys::QueueError> {
    let now = input_ticks();
    let at = keys::message_time(message, now)?;
    Ok((at, Duration::from_millis(now - at + keys::TICK_MARGIN_MS)))
}

/// Read before any nested message lookup or native coordinate conversion.
fn current_message_time() -> u32 {
    // SAFETY: the creation time of this thread's current queued message.
    unsafe { GetMessageTime() }.cast_unsigned()
}

fn remember_activation() {
    let at = input_ticks();
    let activation = control::activation_scope().map(|(lifetime, attempt)| Activation {
        lifetime,
        attempt,
        at,
    });
    ACTIVATION_PENDING.with(|pending| {
        // Repeated notifications cannot give an unconsumed return a new age.
        if pending.get().is_none() {
            pending.set(activation);
        }
    });
}

fn key_result(result: InputResult) -> Result<bool, ()> {
    match result {
        // A key deferred across an app switch is dropped: the switch already
        // released every press the host saw, and nothing is replayed later.
        InputResult::Sent | InputResult::Pending | InputResult::Inactive => Ok(true),
        InputResult::Busy => Ok(false),
        _ => Err(()),
    }
}

fn sync_file_focus(viewer: &mut Viewer, hwnd: HWND) {
    if upload::picker_focus_lost(control::lifetime()) {
        ACTIVATION_PENDING.with(|pending| pending.set(None));
        control::reflect_hidden();
        text::deactivate();
        viewer.text_deactivated();
        // Auxiliary source cleanup can delay this negative notification until
        // after a genuine return to the viewer. Preserve that actual current
        // observation through a new scoped pump check; the retired grant is
        // never restored, and no old activation message is reused.
        // SAFETY: scalar comparison against this still-live original viewer.
        if unsafe { GetForegroundWindow() } == hwnd {
            remember_activation();
        }
    }
}

impl Viewer {
    fn retire_file_choice(&mut self) {
        upload::retire_choice();
        if self.choosing_file() {
            self.file_cleanup_started.get_or_insert_with(input_ticks);
        }
    }

    /// The original viewer may finish before the native cancellation timer
    /// returns. Keep collecting that exact worker without blocking the pump,
    /// but never grant a later cleanup interval after timeout or clock change.
    fn pending_file_cleanup(&mut self) -> bool {
        self.retire_file_choice();
        if !self.choosing_file() {
            return false;
        }
        !self.file_cleanup_expired()
    }

    /// Check independently of `choosing_file`: a poll may have just collected
    /// the original worker after its deadline, which cannot repair cleanup.
    fn file_cleanup_expired(&mut self) -> bool {
        let Some(started) = self.file_cleanup_started else {
            return false;
        };
        let now = input_ticks();
        if now < started || now - started >= 10_000 {
            if let Some(menu) = &mut self.upload_menu {
                menu.fail_cleanup();
            }
            return true;
        }
        false
    }

    fn choosing_file(&self) -> bool {
        self.upload_menu
            .as_ref()
            .is_some_and(upload::Menu::choosing)
    }

    fn can_choose_file(&self) -> bool {
        !self.closing
            && !self.reopen.pending()
            && !self.sizing
            && self.mapping.is_some()
            && self.text.is_none()
            && self.display_picker.is_none()
            && self.keys.is_empty()
            && !self.buttons.iter().any(|held| *held)
            && control::visible_now()
            && control::display_ready(self.session.shared())
            && self.session.shared().controlled()
    }

    fn open_file(&mut self, message: u32) {
        if self.choosing_file() || !self.can_choose_file() {
            return;
        }
        // Native selection runs on its own UI thread; this thread continues
        // presenting fresh pictures and servicing the original session.
        self.release_everything();
        let available = self.can_choose_file();
        if let Some(menu) = &mut self.upload_menu
            && !menu.open_choice(
                &self.session.shared_handle(),
                control::lifetime(),
                message,
                available,
            )
        {
            eprintln!("fr-windows: file_choice_expired_or_unavailable");
        }
    }

    fn can_change_display(&self) -> bool {
        use fr_mobile_ffi::session::ControlState;
        !self.closing
            && self.reopen.available()
            && !self.sizing
            && self.mapping.is_some()
            && self.text.is_none()
            && self.display_picker.is_none()
            && !self.choosing_file()
            && self.keys.is_empty()
            && !self.buttons.iter().any(|held| *held)
            && control::visible_now()
            && control::display_ready(self.session.shared())
            && matches!(
                self.session.shared().control_state(),
                ControlState::Unavailable | ControlState::Viewing | ControlState::Controlled
            )
    }

    /// The independent native handler has already fenced this exact owner.
    /// A later lifecycle event may still cancel replacement, never revive it.
    fn replace_requested(&mut self, cause: Cause, request: &change_display::Request<Shared>) {
        if !Arc::ptr_eq(&request.original, &self.session.shared_handle()) {
            return;
        }
        if self.closing
            || self.sizing
            || (cause == Cause::Display
                && (self.mapping.is_none()
                    || self.text.is_some()
                    || self.display_picker.is_some()
                    || self.choosing_file()))
            || (cause != Cause::FocusLost && !control::visible_now())
            || !request.current(
                &self.session.shared_handle(),
                control::lifetime(),
                self.session.shared().frames.app_id(0),
            )
        {
            self.reopen_refusal.get_or_insert(Refusal::Cancelled);
            self.close();
            return;
        }
        if let Err(reason) = self.reopen.request(cause, request.lifetime, request.at) {
            self.reopen_refusal.get_or_insert(reason);
            self.close();
            return;
        }
        self.reopen_asked = Some(Instant::now());
        self.retire_file_choice();
        self.keys.clear();
        self.buttons.fill(false);
        self.mapping = None;
        self.text = None;
        // SAFETY: this UI thread's mouse capture only, after the original
        // session's input and clipboard owners were irrevocably fenced.
        let _ = unsafe { ReleaseCapture() };
        if cause == Cause::FocusLost {
            self.picture_drawn = None;
            self.repaint_pending = true;
            if self.renderer.clear().is_err() {
                self.session.shared().presentation_failed();
                self.reopen_refusal.get_or_insert(Refusal::Presentation);
                self.close();
            } else if self.renderer.present_accepted() {
                self.cursor_drawn = None;
            }
        }
    }

    fn request_control(&mut self, message: u32) {
        if self.closing
            || self.reopen.pending()
            || self.text.is_some()
            || self.display_picker.is_some()
            || self.choosing_file()
            || self.mapping.is_none()
            || !self.keys.is_empty()
            || self.buttons.iter().any(|held| *held)
        {
            return;
        }
        if !self.input_menu.request(self.session.shared(), message) {
            eprintln!("fr-windows: control_choice_expired_or_unavailable");
        }
        self.input_menu.refresh(self.session.shared());
    }

    fn update_display_picker(&mut self, hwnd: HWND) {
        if self
            .display_picker
            .as_ref()
            .is_some_and(|picker| !picker.current())
        {
            self.display_picker = None;
        }
        if self.display_picker.is_some() || self.closing || self.reopen.pending() {
            return;
        }
        let Some(request) = self.session.shared().display_request() else {
            return;
        };
        if let Ok(picker) = display::Picker::open(hwnd, self.session.shared_handle(), request) {
            self.display_picker = Some(picker);
        } else {
            eprintln!("fr-windows: display_picker_unavailable");
            self.close();
        }
    }
    fn open_text(&mut self, hwnd: HWND) {
        if self.closing
            || self.reopen.pending()
            || self.display_picker.is_some()
            || self.choosing_file()
        {
            return;
        }
        // Reopening is a new explicit local choice. No content or context from
        // an earlier editor is copied, including after a refused Send.
        self.text = None;
        let (draft, notice) = if self.keys.is_empty() {
            match self.session.shared().begin_text() {
                Ok(draft) => (
                    Some(draft),
                    "Compose locally; Send transmits the whole value once.",
                ),
                Err(InputResult::Unsupported) => {
                    (None, "This host has not granted direct Unicode text input.")
                }
                Err(InputResult::NotControlled) => (None, "Request control before sending text."),
                Err(_) => (
                    None,
                    "Text is not ready. Release input and reopen Send text.",
                ),
            }
        } else {
            (
                None,
                "Earlier keyboard input is still pending. Reopen Send text after it settles.",
            )
        };
        // Creating an owned editor changes native focus. Reconcile holds now;
        // the resulting WM_KILLFOCUS may reenter this already-borrowed Viewer.
        self.release_everything();
        match text::Editor::open(hwnd, self.session.shared_handle(), draft, notice) {
            Ok(editor) => self.text = Some(editor),
            Err(_) => eprintln!("fr-windows: text_editor_unavailable"),
        }
    }

    fn send_text(&mut self, message_time: u32) {
        let Some(editor) = self.text.as_mut() else {
            return;
        };
        editor.send(
            message_time,
            !self.closing && !self.reopen.pending() && self.keys.is_empty(),
        );
    }

    /// The independent native fence has already stopped the exact original
    /// source before this cleanup, even if an OS call reentered a busy Viewer.
    fn text_deactivated(&mut self) {
        if self.text.is_some() {
            self.text = None;
            self.release_everything();
        }
    }

    /// The picture point under `(x, y)`. While a button is held the window
    /// has the mouse captured, and points past the picture's edge clamp to
    /// it, so a drag that leaves the picture still releases.
    fn point(&self, at: u64, x: i32, y: i32) -> Result<Option<(f64, f64)>, StaleMapping> {
        self.mapping
            .as_ref()
            .ok_or(StaleMapping)?
            .point(at, x, y, self.buttons.iter().any(|b| *b))
    }
    /// Capture the original grant before any native conversion or key drain.
    /// Replaceable old motion may be discarded; losing an ordered transition
    /// instead fences control and delegates held-state cleanup to its owner.
    fn capture_mouse(&mut self, message_time: u32, ordered: bool) -> Option<(CapturedInput, u64)> {
        if self.closing || self.reopen.pending() || self.text.is_some() || self.choosing_file() {
            return None;
        }
        let capture = match self.session.shared().capture_input() {
            Ok(capture) => capture,
            Err(InputResult::NotControlled | InputResult::Inactive) => return None,
            Err(_) => {
                self.input_failed();
                return None;
            }
        };
        let aged = message_age(message_time)
            .ok()
            .and_then(|(at, age)| capture.with_queue_age(age).map(|capture| (capture, at)));
        let Some(captured) = aged else {
            if ordered {
                self.input_failed();
            }
            return None;
        };
        self.after_keys(ordered).then_some(captured)
    }
    fn pointer(&mut self, x: i32, y: i32, message_time: u32) {
        // Replaceable motion may wait for a busy key; it must not overtake
        // the key and give frd a newer capture stamp ahead of that key.
        let Some((captured, at)) = self.capture_mouse(message_time, false) else {
            return;
        };
        if let Ok(Some((px, py))) = self.point(at, x, y) {
            match self.session.shared().captured_pointer(captured, px, py) {
                InputResult::Sent => self.last_point = (px, py),
                InputResult::ControlLost => self.input_failed(),
                _ => {}
            }
        }
    }
    fn button(
        &mut self,
        hwnd: HWND,
        index: usize,
        pressed: bool,
        (x, y): (i32, i32),
        message_time: u32,
    ) {
        let Some((captured, at)) = self.capture_mouse(message_time, true) else {
            return;
        };
        let (px, py) = match self.point(at, x, y) {
            Ok(Some(point)) => point,
            Ok(None) => return,
            Err(StaleMapping) => {
                self.input_failed();
                return;
            }
        };
        let Ok(wire) = u32::try_from(index) else {
            return;
        };
        match self
            .session
            .shared()
            .captured_button(captured, wire, pressed, px, py)
        {
            InputResult::Sent | InputResult::Pending => {}
            InputResult::NotControlled
            | InputResult::Inactive
            | InputResult::Unsupported
            | InputResult::Outside => return,
            _ => {
                self.input_failed();
                return;
            }
        }
        self.last_point = (px, py);
        self.buttons[index] = pressed;
        // SAFETY: capture for this thread's own window while any button is
        // held, released when none is.
        unsafe {
            if pressed {
                SetCapture(hwnd);
            } else if !self.buttons.iter().any(|b| *b) {
                let _ = ReleaseCapture();
            }
        }
    }
    /// A key stamped once, in order behind any the session deferred. The
    /// source clock is sampled before the native age, conservatively charging
    /// an interrupted sample or a wait for the grant lock to the old event.
    fn key(&mut self, usage: u16, pressed: bool, message_time: u32) {
        if self.closing || self.reopen.pending() || self.choosing_file() {
            return;
        }
        let shared = self.session.shared();
        let captured = match shared.capture_key(usage, pressed) {
            Ok(captured) => captured,
            // Queued before an app switch: dropped, never failed or replayed.
            Err(InputResult::Inactive) => return,
            Err(_) => {
                self.input_failed();
                return;
            }
        };
        let Ok((at, age)) = message_age(message_time) else {
            self.input_failed();
            return;
        };
        let Some(captured) = captured.with_queue_age(age) else {
            self.input_failed();
            return;
        };
        if self
            .keys
            .push(captured, at, input_ticks, |captured| {
                key_result(shared.captured_key(captured))
            })
            .is_err()
        {
            self.input_failed();
        }
    }
    /// Retry the same captures; neither the native queue nor frd receives a
    /// fresh deadline when transport pacing releases a slot.
    fn send_keys(&mut self) {
        let shared = self.session.shared();
        if self
            .keys
            .drain(input_ticks, |captured| {
                key_result(shared.captured_key(captured))
            })
            .is_err()
        {
            self.input_failed();
        }
    }
    /// An ordered button/scroll cannot bypass an unsent key. If the bounded
    /// source still refuses it, fence and clean up instead of reordering input.
    fn after_keys(&mut self, ordered: bool) -> bool {
        if self.closing || self.reopen.pending() {
            return false;
        }
        self.send_keys();
        if self.closing {
            return false;
        }
        if self.keys.is_empty() {
            return true;
        }
        if ordered {
            self.input_failed();
        }
        false
    }
    fn scroll(&mut self, hwnd: HWND, (dx, dy): (f64, f64), (x, y): (i32, i32), message_time: u32) {
        let Some((captured, at)) = self.capture_mouse(message_time, true) else {
            return;
        };
        // Wheel points are screen coordinates. Retain the original source
        // and deadline before this call, and never guess after native failure.
        let mut point = POINT { x, y };
        // SAFETY: this thread's live window and a writable point.
        if !unsafe { ScreenToClient(hwnd, &raw mut point) }.as_bool() {
            self.input_failed();
            return;
        }
        match self.point(at, point.x, point.y) {
            Ok(Some((px, py))) => {
                if self
                    .session
                    .shared()
                    .captured_scroll(captured, dx, dy, px, py)
                    == InputResult::ControlLost
                {
                    self.input_failed();
                }
            }
            Ok(None) => {}
            Err(StaleMapping) => self.input_failed(),
        }
    }
    fn input_failed(&mut self) {
        // Mark the local owner terminal before stopping its source. Dropping
        // that source wakes frd's independent held-state cleanup; attempting
        // more keys or re-stamping a release is never the recovery path.
        self.closing = true;
        self.retire_file_choice();
        self.reopen.cancel();
        self.keys.clear();
        self.session.shared().input_failed();
        self.text = None;
        self.buttons.fill(false);
        // SAFETY: release only this UI thread's mouse capture, after the
        // original remote source has been fenced.
        let _ = unsafe { ReleaseCapture() };
    }
    /// Closes for good: no reopen follows.
    fn close(&mut self) {
        self.closing = true;
        self.retire_file_choice();
        self.reopen.cancel();
        self.keys.clear();
        self.session.shared().close();
        self.text = None;
        self.display_picker = None;
    }
    /// The client area is now `size` (a drag ended, or a maximize or
    /// restore): reopen when the host would send a different picture.
    fn resized(&mut self, size: (u32, u32)) {
        self.text = None;
        if self.closing
            || self.choosing_file()
            || !self.reopen.available()
            || self.scales != Some(true)
            || !reopen_for(self.display, self.picture, size)
        {
            return;
        }
        if self
            .reopen
            .request(Cause::Resize, control::lifetime(), input_ticks())
            .is_err()
        {
            return;
        }
        self.reopen_asked = Some(Instant::now());
        self.release_everything();
        self.session.shared().close();
    }
    fn release_everything(&mut self) {
        // Unsent presses never go after focus loss; held keys are released.
        self.keys.clear();
        let shared = self.session.shared();
        let _ = shared.release_all();
        let (px, py) = self.last_point;
        for (index, held) in self.buttons.iter_mut().enumerate() {
            if *held {
                if let Ok(wire) = u32::try_from(index) {
                    let _ = shared.button(wire, false, px, py);
                }
                *held = false;
            }
        }
    }
}

#[allow(clippy::too_many_lines, reason = "one message switch")]
extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let handled = match message {
        WM_SETCURSOR
            if u32::try_from(lparam.0 & 0xffff) == Ok(HTCLIENT) && wparam.0 == hwnd.0 as usize =>
        {
            // A nested native draw can temporarily borrow Viewer. Preserve
            // the current pointer then, rather than reinstalling an arrow
            // over an overlay whose owner blanked it before presentation.
            with_viewer(|v| cursor::refresh(v, hwnd));
            return LRESULT(1);
        }
        WM_INITMENUPOPUP => {
            with_viewer(|v| {
                let file_available = v.can_choose_file() && !v.choosing_file();
                if let Some(menu) = &mut v.upload_menu {
                    menu.refresh();
                    if menu.is_popup(wparam.0) {
                        menu.opened(
                            &v.session.shared_handle(),
                            control::lifetime(),
                            file_available,
                        );
                    }
                }
                if v.input_menu.is_popup(wparam.0) {
                    let available = v.can_change_display();
                    v.input_menu.display_available(available);
                    v.input_menu.opened(&v.session.shared_handle());
                }
            });
            false
        }
        WM_COMMAND if wparam.0 & 0xffff == upload::OPEN => {
            let time = current_message_time();
            with_viewer(|v| v.open_file(time));
            true
        }
        WM_COMMAND if wparam.0 & 0xffff == upload::CANCEL => {
            // Stop the original file even during a nested native callback.
            // This cannot request control, close the desktop, or start a retry.
            upload::cancel();
            true
        }
        WM_COMMAND if wparam.0 & 0xffff == upload::PAUSE => {
            upload::pause();
            true
        }
        WM_COMMAND if wparam.0 & 0xffff == upload::RESUME => {
            upload::resume();
            true
        }
        WM_COMMAND if wparam.0 & 0xffff == control::REQUEST => {
            let time = current_message_time();
            with_viewer(|v| v.request_control(time));
            true
        }
        WM_COMMAND if wparam.0 & 0xffff == control::STOP => {
            // Fence the popup's original owner before borrowing Viewer or
            // clearing native composition. No pending input can survive Stop.
            let time = current_message_time();
            if let Some(request) = control::stop(time) {
                CHANGE_PENDING.with(|pending| {
                    *pending.borrow_mut() = Some((Cause::StopControl, request));
                });
                drain_close();
            } else {
                eprintln!("fr-windows: control_stop_expired_or_unavailable");
            }
            true
        }
        WM_COMMAND if wparam.0 & 0xffff == control::CHANGE_DISPLAY => {
            let time = current_message_time();
            if let Some(request) = control::change_display(time) {
                CHANGE_PENDING.with(|pending| {
                    *pending.borrow_mut() = Some((Cause::Display, request));
                });
                drain_close();
            } else {
                eprintln!("fr-windows: display_choice_expired_or_unavailable");
            }
            true
        }
        WM_COMMAND if wparam.0 & 0xffff == text::OPEN => {
            control::invalidate_display();
            with_viewer(|v| v.open_text(hwnd));
            true
        }
        WM_ACTIVATEAPP if wparam.0 == 0 => {
            ACTIVATION_PENDING.with(|pending| pending.set(None));
            let picker = u32::try_from(lparam.0)
                .is_ok_and(|thread| upload::picker_deactivated(thread, control::lifetime()));
            if !picker {
                upload::retire_choice();
                // An app switch pauses control instead of ending it. Release
                // what this window holds while it can still send; the shared
                // owner then releases anything a borrowed Viewer could not,
                // and admits nothing until the pump sees this window in front.
                with_viewer(Viewer::release_everything);
                if control::active(false) {
                    text::deactivate_app();
                } else {
                    // A refused release must not leave input paused with
                    // state held on the host: end this owner instead, in
                    // order when it can be (its cleanup releases held input;
                    // the window stays for a fresh viewing session).
                    if let Some(request) = control::focus_lost() {
                        CHANGE_PENDING.with(|pending| {
                            *pending.borrow_mut() = Some((Cause::FocusLost, request));
                        });
                        drain_close();
                    } else {
                        control::visible(false);
                    }
                    text::deactivate();
                }
                with_viewer(Viewer::text_deactivated);
                with_viewer(|v| {
                    v.picture_drawn = None;
                    v.repaint_pending = true;
                    cursor::refresh(v, hwnd);
                });
            }
            false
        }
        WM_ACTIVATEAPP => {
            remember_activation();
            false
        }
        WM_CLOSE => {
            upload::retire_choice();
            CLOSE_PENDING.with(|pending| pending.set(true));
            display::close_owner();
            control::close();
            text::close_owner();
            drain_close();
            // An orderly close: the loop ends once the session finishes.
            // A borrowed viewer retains the coalesced close for the pump;
            // only an absent owner permits destroying its HWND here.
            if VIEWER.with(|cell| cell.try_borrow().is_ok_and(|v| v.is_none())) {
                CLOSE_PENDING.with(|pending| pending.set(false));
                // SAFETY: no session: destroy this thread's window now.
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            true
        }
        WM_DESTROY => {
            upload::retire_choice();
            control::close();
            display::close_owner();
            text::close_owner();
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            true
        }
        WM_SIZE => {
            upload::retire_choice();
            // Fence an open editor before any nested native work or Viewer
            // borrow; old text cannot outlive hiding or a mapping change.
            text::deactivate();
            control::invalidate_choice();
            let (width, height) = point_of(lparam);
            let minimized = wparam.0 == SIZE_MINIMIZED as usize;
            if minimized {
                control::visible(false);
            }
            // Maximize and restore arrive outside any drag; a drag's own
            // sizes wait for WM_EXITSIZEMOVE.
            let settled = matches!(u32::try_from(wparam.0), Ok(SIZE_MAXIMIZED | SIZE_RESTORED));
            with_viewer(|v| {
                if minimized {
                    v.mapping = None;
                    v.session.shared().set_visible(false);
                    v.text = None;
                } else {
                    // A restore/resize alone cannot cancel an earlier native
                    // application-deactivation fence. Activation proves it.
                    v.session.shared().set_visible(control::visible_now());
                    let size = (
                        u32::try_from(width).unwrap_or(0),
                        u32::try_from(height).unwrap_or(0),
                    );
                    // Invalidate before the native resize, including a return
                    // to an earlier size. Old queued coordinates never acquire
                    // the subsequently redrawn mapping.
                    v.mapping = None;
                    v.picture_drawn = None;
                    v.repaint_pending = true;
                    v.text = None;
                    v.session.shared().scroll_end();
                    if v.renderer.resize(size).is_err() {
                        v.session.shared().presentation_failed();
                        v.close();
                        return;
                    }
                    if settled && !v.sizing {
                        v.resized(size);
                    }
                }
            });
            true
        }
        WM_MOVE => {
            upload::retire_choice();
            control::invalidate_choice();
            // Wheel messages carry screen coordinates: moving the window
            // changes their conversion even when its client size is identical.
            with_viewer(|v| {
                v.mapping = None;
                v.session.shared().scroll_end();
            });
            false
        }
        WM_ENTERSIZEMOVE => {
            upload::retire_choice();
            control::invalidate_choice();
            text::deactivate();
            with_viewer(|v| v.sizing = true);
            false
        }
        WM_EXITSIZEMOVE => {
            let size = client_size(hwnd);
            with_viewer(|v| {
                v.sizing = false;
                v.resized(size);
            });
            false
        }
        WM_ERASEBKGND => return LRESULT(1),
        WM_PAINT => {
            // SAFETY: the loop draws every picture; nothing to paint here.
            let _ = unsafe { ValidateRect(Some(hwnd), None) };
            true
        }
        WM_MOUSEMOVE => {
            let time = current_message_time();
            let (x, y) = point_of(lparam);
            with_viewer(|v| v.pointer(x, y, time));
            true
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let time = current_message_time();
            let index = match message {
                WM_LBUTTONDOWN | WM_LBUTTONUP => 0,
                WM_RBUTTONDOWN | WM_RBUTTONUP => 1,
                WM_MBUTTONDOWN | WM_MBUTTONUP => 2,
                // XBUTTON1 is Back, XBUTTON2 Forward (HIWORD of wParam).
                _ if (wparam.0 >> 16) & 0xffff == 2 => 4,
                _ => 3,
            };
            let pressed = matches!(
                message,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            );
            let (x, y) = point_of(lparam);
            with_viewer(|v| v.button(hwnd, index, pressed, (x, y), time));
            if matches!(message, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                return LRESULT(1);
            }
            true
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let time = current_message_time();
            #[allow(clippy::cast_possible_wrap, reason = "GET_WHEEL_DELTA_WPARAM")]
            let delta = f64::from(words(wparam.0 as isize).1) / 120.0;
            // Wheel points are screen coordinates.
            let (sx, sy) = point_of(lparam);

            // Forward (positive) is up; the session's positive y is down.
            // A right tilt is positive x.
            let (dx, dy) = if message == WM_MOUSEWHEEL {
                (0.0, -delta)
            } else {
                (delta, 0.0)
            };
            with_viewer(|v| v.scroll(hwnd, (dx, dy), (sx, sy), time));
            true
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let (scan, extended) = key_bits(lparam);
            let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            // Read BEFORE peeking at another message for AltGr detection.
            let time = current_message_time();
            // Bit 30: the key was already down (auto-repeat). The session
            // never sends repeats; the host repeats a held key itself.
            let repeat = pressed && (lparam.0 >> 30) & 1 == 1;
            // The left Ctrl Windows adds around AltGr never reaches the host
            // (keys::synthetic_ctrl); AltGr itself goes as right Alt.
            let synthetic = scan == 0x1D && !extended && {
                let this = keys::KeyMessage {
                    scan,
                    extended,
                    pressed,
                    time,
                };
                keys::synthetic_ctrl(this, next_key(hwnd))
            };
            // Only a controlled session takes keys; otherwise Windows keeps
            // its own (Alt+F4 and the system menu).
            with_viewer(|v| {
                if v.text.is_some() || !v.session.shared().controlled() {
                    return false;
                }
                if let Some(usage) = fr_core::input::windows_scan::usage(scan, extended)
                    && !repeat
                    && !synthetic
                {
                    v.key(usage, pressed, time);
                }
                true
            })
            .unwrap_or(false)
        }
        WM_KILLFOCUS => {
            with_viewer(Viewer::release_everything);
            false
        }
        _ => false,
    };
    if handled {
        LRESULT(0)
    } else {
        // SAFETY: default processing of this window's message.
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }
}
