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
//! original requested close; it resumes viewing only. Stopping an active
//! input source cancels its owner, so controlled resize cannot prove that
//! orderly close and may terminate instead. Control requires a fresh
//! explicit invocation, never replay of the earlier --control choice.
use super::render::Renderer;
use crate::{
    geometry::{Rect, host_scales, picture_point, reopen_for},
    keys,
};
use fr_mobile_ffi::{
    events::{Event, kind, state},
    session::{CapturedKey, Config, InputResult, Session, end},
    video,
};
use std::{
    cell::RefCell,
    fmt::Write as _,
    io::Write as _,
    process::ExitCode,
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
    GetMessageTime, IDC_ARROW, LoadCursorW, MSG, MsgWaitForMultipleObjects, PM_NOREMOVE, PM_REMOVE,
    PeekMessageW, PostQuitMessage, QS_ALLINPUT, RegisterClassExW, SIZE_MAXIMIZED, SIZE_MINIMIZED,
    SIZE_RESTORED, SPI_GETWORKAREA, SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, ShowWindow,
    SystemParametersInfoW, TranslateMessage, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY,
    WM_ENTERSIZEMOVE, WM_ERASEBKGND, WM_EXITSIZEMOVE, WM_KEYDOWN, WM_KEYFIRST, WM_KEYLAST,
    WM_KEYUP, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_QUIT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SIZE, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW,
    WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, Interface, w};

/// The window's session, renderer and input state, owned by the UI thread.
struct Viewer {
    session: Session,
    renderer: Renderer,
    /// The last drawn picture's size and place (input maps through these).
    picture: (u32, u32),
    dest: Option<Rect>,
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
    reopen: bool,
    /// Closing for good (the user or `--seconds`): no more reopens.
    closing: bool,
    /// The box the current session stated (its client size at start).
    stated: (u32, u32),
    /// Whether this host fits pictures into the stated box, once a picture
    /// has shown it (`geometry::host_scales`).
    scales: Option<bool>,
    /// When the pending reopen was asked for (its gap is reported).
    reopen_asked: Option<Instant>,
    /// When the pending reopen dials (`REDIAL_PAUSE` after the old session
    /// finished).
    redial_at: Option<Instant>,
}
thread_local! {
    static VIEWER: RefCell<Option<Viewer>> = const { RefCell::new(None) };
}
/// Runs `f` on the viewer unless it is absent or already borrowed (a
/// message dispatched from inside another viewer call).
fn with_viewer<R>(f: impl FnOnce(&mut Viewer) -> R) -> Option<R> {
    VIEWER.with(|cell| cell.try_borrow_mut().ok()?.as_mut().map(f))
}

#[derive(Default)]
struct Stats {
    /// Sessions reopened for a new picture size.
    reopens: u64,
    /// Counts of the sessions before the current one.
    earlier_decoded: u64,
    earlier_decode_micros: u64,
    earlier_presented: u64,
    /// The first session's time to its first view.
    first_view_micros: Option<u64>,
    /// Longest wait from a reopen's request to its first drawn picture.
    reopen_gap_micros: u64,
    drawn: u64,
    draw_failures: u64,
    last_draw: Option<Instant>,
    /// Microseconds between consecutive draws.
    intervals: Vec<u64>,
    picture: (u32, u32),
    display: (u32, u32),
}

pub fn run(mut config: Config, title: &str, seconds: Option<u64>) -> ExitCode {
    // SAFETY: process-wide DPI awareness, set before any window exists, so
    // client-area sizes and mouse points are physical pixels.
    let _ = unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let Some(gpu) = video::gpu() else {
        return refuse("no_d3d11_device");
    };
    let Ok(hwnd) = create_window(title) else {
        return refuse("window_unavailable");
    };
    let size = client_size(hwnd);
    config.picture = Some(size);
    let control = config.control;
    let Ok(session) = Session::start(config.clone()) else {
        return refuse("session_thread_unavailable");
    };
    let Ok(renderer) = Renderer::new(&gpu.device, hwnd, size) else {
        return refuse("swap_chain_unavailable");
    };
    if control {
        session.shared().request_control();
    }
    VIEWER.with(|cell| {
        *cell.borrow_mut() = Some(Viewer {
            session,
            renderer,
            picture: (0, 0),
            dest: None,
            buttons: [false; 5],
            last_point: (0.0, 0.0),
            keys: keys::Deferred::default(),
            display: (0, 0),
            sizing: false,
            reopen: false,
            closing: false,
            stated: size,
            scales: None,
            reopen_asked: None,
            redial_at: None,
        });
    });
    // SAFETY: showing the window this thread created.
    let _ = unsafe { ShowWindow(hwnd, SW_SHOW) };
    with_viewer(|v| v.session.shared().set_visible(true));
    let started = Instant::now();
    let stats = pump(seconds, hwnd, &config);
    let viewer = VIEWER.with(|cell| cell.borrow_mut().take());
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
        // SAFETY: the standard message pump on this thread's queue.
        while unsafe { PeekMessageW(&raw mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            if msg.message == WM_QUIT {
                return stats;
            }
            // SAFETY: a message this thread just removed from its queue.
            unsafe {
                let _ = TranslateMessage(&raw const msg);
                DispatchMessageW(&raw const msg);
            }
        }
        let Some((finished, drew)) = with_viewer(|v| step(v, &mut stats, &mut last_sequence))
        else {
            return stats;
        };
        if finished {
            match with_viewer(|v| reopen(v, &mut stats, hwnd, config)) {
                Some(Reopen::Started) => {
                    last_sequence = 0;
                    continue;
                }
                Some(Reopen::Waiting) => {}
                Some(Reopen::No) | None => return stats,
            }
        }
        if !closing && deadline.is_some_and(|d| Instant::now() >= d) {
            closing = true;
            with_viewer(Viewer::close);
        }
        if !drew {
            // SAFETY: waits for input or 2 ms; no handles.
            unsafe { MsgWaitForMultipleObjects(None, false, 2, QS_ALLINPUT) };
        }
    }
}

/// Replaces a session that finished because a resize closed it with one
/// stating the current client size, once `REDIAL_PAUSE` has passed. `No`:
/// no reopen was asked for (or it could not start), so the viewer ends.
fn reopen(v: &mut Viewer, stats: &mut Stats, hwnd: HWND, config: &Config) -> Reopen {
    if !v.reopen || v.closing || !v.session.shared().orderly_closed() {
        return Reopen::No;
    }
    let now = Instant::now();
    if now < *v.redial_at.get_or_insert(now + REDIAL_PAUSE) {
        return Reopen::Waiting;
    }
    v.redial_at = None;
    v.reopen = false;
    let mut next = config.clone();
    // The earlier input consent ended with its original grant. A resize
    // may reopen a view but cannot request or restore control automatically.
    next.control = false;
    let size = client_size(hwnd);
    next.picture = Some(size);
    let Ok(session) = Session::start(next) else {
        return Reopen::No;
    };
    v.stated = size;
    let old = std::mem::replace(&mut v.session, session);
    let shared = old.shared();
    let (decoded, decode_micros) = shared.frames.decode_stats();
    stats.earlier_decoded += decoded;
    stats.earlier_decode_micros += decode_micros;
    stats.earlier_presented += shared.frames.presented();
    stats
        .first_view_micros
        .get_or_insert(shared.first_view_micros());
    stats.reopens += 1;
    // The reopen's own wait is not a frame interval.
    stats.last_draw = None;
    v.dest = None;
    v.picture = (0, 0);
    v.buttons = [false; 5];
    v.keys = keys::Deferred::default();
    v.session.shared().set_visible(true);
    Reopen::Started
}

enum Reopen {
    Started,
    /// The finished session's host is still rebinding: dial a little later.
    Waiting,
    No,
}

/// frd's serial host binds its next listener only after the last attempt's
/// retirement wait and a 250 ms cooldown; a dial in that gap loses its
/// Initials and connects about 5 s later (fr-w4yu). The iOS app waits 400 ms
/// before a session that follows another; so does a reopen.
const REDIAL_PAUSE: Duration = Duration::from_millis(400);

/// Events, then the newest picture if there is one. (finished, drew)
fn step(v: &mut Viewer, stats: &mut Stats, last_sequence: &mut u64) -> (bool, bool) {
    if !v.keys.is_empty() {
        v.send_keys();
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
        return (true, false);
    }
    let Some((raw, info)) = shared
        .frames
        .take_newer(*last_sequence, shared.full_range())
    else {
        return (false, false);
    };
    *last_sequence = info.sequence;
    // SAFETY: the slot handed out a +1 `IMFSample` reference for this
    // caller; `from_raw` takes it over.
    let sample = unsafe { IMFSample::from_raw(raw) };
    let drawn = video::texture_of(&sample).and_then(|(texture, slice)| {
        v.renderer
            .draw(&texture, slice, (info.width, info.height), info.full_range)
            .ok()
            .flatten()
    });
    let Some(dest) = drawn else {
        stats.draw_failures += 1;
        return (false, false);
    };
    v.dest = Some(dest);
    v.picture = (info.width, info.height);
    if v.scales.is_none() {
        v.scales = host_scales(v.display, v.stated, v.picture);
    }
    shared.frames.mark_drawn(info.frame);
    let now = Instant::now();
    if let Some(asked) = v.reopen_asked.take() {
        let gap = u64::try_from(now.duration_since(asked).as_micros()).unwrap_or(u64::MAX);
        stats.reopen_gap_micros = stats.reopen_gap_micros.max(gap);
    }
    if let Some(last) = stats.last_draw {
        stats
            .intervals
            .push(u64::try_from(now.duration_since(last).as_micros()).unwrap_or(u64::MAX));
    }
    stats.last_draw = Some(now);
    stats.drawn += 1;
    stats.picture = v.picture;
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
    let mut intervals = stats.intervals.clone();
    intervals.sort_unstable();
    let percentile = |p: usize| -> f64 {
        if intervals.is_empty() {
            return 0.0;
        }
        let index = (intervals.len() - 1) * p / 100;
        micros_to_ms(intervals[index])
    };
    let mean_decode = decode_micros.checked_div(decoded).map_or(0.0, micros_to_ms);
    let mut json = String::new();
    let _ = write!(
        json,
        "{{\"status\":\"ended\",\"end\":\"{}\",\"detail\":{},\"drawn\":{},\"draw_failures\":{},\
         \"presented\":{},\"decoded\":{},\"decode_ms_mean\":{:.2},\"first_view_ms\":{:.1},\
         \"dxva\":{},\"display\":[{},{}],\"picture\":[{},{}],\"window\":[{},{}],\
         \"frame_interval_ms\":{{\"p50\":{:.1},\"p95\":{:.1},\"p99\":{:.1},\"max\":{:.1}}},\
         \"reopens\":{},\"reopen_gap_ms\":{:.1},\"last_first_view_ms\":{:.1},\
         \"seconds\":{:.1}}}",
        end_name(code),
        shared
            .detail()
            .map_or_else(|| "null".to_owned(), |d| format!("{d:?}")),
        stats.drawn,
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
        percentile(50),
        percentile(95),
        percentile(99),
        intervals.last().map_or(0.0, |m| micros_to_ms(*m)),
        stats.reopens,
        micros_to_ms(stats.reopen_gap_micros),
        micros_to_ms(shared.first_view_micros()),
        elapsed.as_secs_f64(),
    );
    let _ = writeln!(std::io::stdout().lock(), "{json}");
    if stats.drawn > 0 && matches!(code, end::OK | end::CANCELLED) {
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

fn key_result(result: InputResult) -> Result<bool, ()> {
    match result {
        InputResult::Sent | InputResult::Pending => Ok(true),
        InputResult::Busy => Ok(false),
        _ => Err(()),
    }
}

impl Viewer {
    /// The picture point under `(x, y)`. While a button is held the window
    /// has the mouse captured, and points past the picture's edge clamp to
    /// it, so a drag that leaves the picture still releases.
    fn point(&self, x: i32, y: i32) -> Option<(f64, f64)> {
        let dest = self.dest?;
        let (x, y) = if self.buttons.iter().any(|b| *b) {
            (
                x.clamp(dest.left, dest.right - 1),
                y.clamp(dest.top, dest.bottom - 1),
            )
        } else {
            (x, y)
        };
        picture_point(dest, self.picture, x, y)
    }
    fn pointer(&mut self, x: i32, y: i32) {
        // Replaceable motion may wait for a busy key; it must not overtake
        // the key and give frd a newer capture stamp ahead of that key.
        if !self.after_keys(false) {
            return;
        }
        if let Some((px, py)) = self.point(x, y) {
            self.last_point = (px, py);
            let _ = self.session.shared().pointer(px, py);
        }
    }
    fn button(&mut self, hwnd: HWND, index: usize, pressed: bool, x: i32, y: i32) {
        if !self.after_keys(true) {
            return;
        }
        let Some((px, py)) = self.point(x, y) else {
            return;
        };
        self.last_point = (px, py);
        let Ok(wire) = u32::try_from(index) else {
            return;
        };
        let _ = self.session.shared().button(wire, pressed, px, py);
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
        if self.closing || self.reopen {
            return;
        }
        let shared = self.session.shared();
        let Ok(captured) = shared.capture_key(usage, pressed) else {
            self.input_failed();
            return;
        };
        let now = input_ticks();
        let Ok(at) = keys::message_time(message_time, now) else {
            self.input_failed();
            return;
        };
        let age = Duration::from_millis(now - at + keys::TICK_MARGIN_MS);
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
        if self.closing || self.reopen {
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
    fn scroll(&mut self, dx: f64, dy: f64, x: i32, y: i32) {
        if !self.after_keys(true) {
            return;
        }
        if let Some((px, py)) = self.point(x, y) {
            let _ = self.session.shared().scroll(dx, dy, px, py);
        }
    }
    fn input_failed(&mut self) {
        // Mark the local owner terminal before stopping its source. Dropping
        // that source wakes frd's independent held-state cleanup; attempting
        // more keys or re-stamping a release is never the recovery path.
        self.closing = true;
        self.reopen = false;
        self.keys.clear();
        self.session.shared().input_failed();
        self.buttons.fill(false);
        // SAFETY: release only this UI thread's mouse capture, after the
        // original remote source has been fenced.
        let _ = unsafe { ReleaseCapture() };
    }
    /// Closes for good: no reopen follows.
    fn close(&mut self) {
        self.closing = true;
        self.reopen = false;
        self.keys.clear();
        self.session.shared().close();
    }
    /// The client area is now `size` (a drag ended, or a maximize or
    /// restore): reopen when the host would send a different picture.
    fn resized(&mut self, size: (u32, u32)) {
        if self.closing
            || self.reopen
            || self.scales != Some(true)
            || !reopen_for(self.display, self.picture, size)
        {
            return;
        }
        self.reopen = true;
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
        WM_CLOSE => {
            // An orderly close: the loop ends once the session finishes.
            if with_viewer(Viewer::close).is_none() {
                // SAFETY: no session: destroy this thread's window now.
                let _ = unsafe { DestroyWindow(hwnd) };
            }
            true
        }
        WM_DESTROY => {
            // SAFETY: ends this thread's message loop.
            unsafe { PostQuitMessage(0) };
            true
        }
        WM_SIZE => {
            let (width, height) = point_of(lparam);
            let minimized = wparam.0 == SIZE_MINIMIZED as usize;
            // Maximize and restore arrive outside any drag; a drag's own
            // sizes wait for WM_EXITSIZEMOVE.
            let settled = matches!(u32::try_from(wparam.0), Ok(SIZE_MAXIMIZED | SIZE_RESTORED));
            with_viewer(|v| {
                if minimized {
                    v.session.shared().set_visible(false);
                } else {
                    v.session.shared().set_visible(true);
                    let size = (
                        u32::try_from(width).unwrap_or(0),
                        u32::try_from(height).unwrap_or(0),
                    );
                    let _ = v.renderer.resize(size);
                    if settled && !v.sizing {
                        v.resized(size);
                    }
                }
            });
            true
        }
        WM_ENTERSIZEMOVE => {
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
            let (x, y) = point_of(lparam);
            with_viewer(|v| v.pointer(x, y));
            true
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
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
            with_viewer(|v| v.button(hwnd, index, pressed, x, y));
            if matches!(message, WM_XBUTTONDOWN | WM_XBUTTONUP) {
                return LRESULT(1);
            }
            true
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            #[allow(clippy::cast_possible_wrap, reason = "GET_WHEEL_DELTA_WPARAM")]
            let delta = f64::from(words(wparam.0 as isize).1) / 120.0;
            // Wheel points are screen coordinates.
            let (sx, sy) = point_of(lparam);
            let mut point = POINT { x: sx, y: sy };
            // SAFETY: a live window and a writable point.
            let _ = unsafe { ScreenToClient(hwnd, &raw mut point) };
            // Forward (positive) is up; the session's positive y is down.
            // A right tilt is positive x.
            let (dx, dy) = if message == WM_MOUSEWHEEL {
                (0.0, -delta)
            } else {
                (delta, 0.0)
            };
            with_viewer(|v| v.scroll(dx, dy, point.x, point.y));
            true
        }
        WM_KEYDOWN | WM_SYSKEYDOWN | WM_KEYUP | WM_SYSKEYUP => {
            let (scan, extended) = key_bits(lparam);
            let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            // Read BEFORE peeking at another message for AltGr detection.
            // SAFETY: the creation time of this thread's current message.
            let time = unsafe { GetMessageTime() }.cast_unsigned();
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
                if !v.session.shared().controlled() {
                    return false;
                }
                if let Some(usage) = keys::usage(scan, extended)
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
