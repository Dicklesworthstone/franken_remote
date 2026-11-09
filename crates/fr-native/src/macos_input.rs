//! macOS host input injection (fr-macos-host-control-b14s) for the per-lease
//! executor `fr-input-agent`; frd never loads this. The `macos_input.m` glue
//! posts Quartz events from one private source at the HID tap. A grant names
//! one display in the host's desktop coordinates (its encoded pixels, see
//! `macos_display`), and every point maps back to that display's points with
//! the formula the capture catalog published it with.
//!
//! The owner mirrors the X11 executor's contract: an exact display binding
//! revalidated before every positive operation, refusal instead of clamping,
//! a press only of what it does not hold, a release only of what it holds,
//! and a pessimistic held set recorded for the last-resort release. Quartz
//! has standalone key repeat and scrolling, so neither is split into
//! pairs. `CGEventPost` returns nothing: `Submitted` means the event was
//! created and handed to the window server, never that an application saw it.
use crate::{
    macos_display::{self, MacDisplay},
    macos_input_state::{Clicks, Held, button_index, scroll_lines, scroll_pixels},
    macos_keys,
};
use core::{
    ffi::{c_int, c_void},
    marker::PhantomData,
    ptr::NonNull,
};
use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, ScrollUnit},
    input_submission::{Capabilities, Capability, Operation, PlatformError, Submission},
    limits::ProtocolLimits,
};
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq)]
struct RawDisplay {
    display_id: u32,
    x: i32,
    y: i32,
    pixel_width: u32,
    pixel_height: u32,
    point_width: u32,
    point_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default, PartialEq)]
struct RawInputDisplay {
    display: RawDisplay,
    origin_x: f64,
    origin_y: f64,
}

unsafe extern "C" {
    fn fr_in_access() -> c_int;
    fn fr_in_displays(out: *mut RawInputDisplay, capacity: usize, count: *mut usize) -> c_int;
    fn fr_in_display_check(expected: *const RawInputDisplay) -> c_int;
    fn fr_in_open() -> *mut c_void;
    fn fr_in_close(source: *mut c_void);
    fn fr_in_key(
        source: *mut c_void,
        keycode: u16,
        down: c_int,
        repeat: c_int,
        modifier: c_int,
        flags: u64,
    ) -> c_int;
    fn fr_in_location(x: *mut f64, y: *mut f64) -> c_int;
    fn fr_in_move(source: *mut c_void, x: f64, y: f64, held: c_int, flags: u64) -> c_int;
    fn fr_in_button(
        source: *mut c_void,
        button: c_int,
        pressed: c_int,
        clicks: i64,
        x: f64,
        y: f64,
        flags: u64,
    ) -> c_int;
    fn fr_in_scroll(
        source: *mut c_void,
        lines_y: i32,
        lines_x: i32,
        x: f64,
        y: f64,
        flags: u64,
    ) -> c_int;
    fn fr_in_pixel_scroll(
        source: *mut c_void,
        fixed_y: i32,
        fixed_x: i32,
        x: f64,
        y: f64,
        flags: u64,
    ) -> c_int;
    fn fr_in_double_click_interval() -> f64;
    fn fr_in_record(keys_low: u64, keys_high: u64, buttons: u32);
    fn fr_in_emergency_release() -> c_int;
    fn fr_in_bind_parent(expected: i32) -> c_int;
    fn fr_in_route_to_process(pid: i32);
    fn fr_in_indicator_open(display_id: u32, timeout_ms: u32) -> c_int;
    fn fr_in_indicator_pump(timeout_ms: u32) -> c_int;
    fn fr_in_indicator_close();
}

fn status(code: c_int) -> Result<(), PlatformError> {
    match code {
        0 => Ok(()),
        -1 | -9 => Err(PlatformError::Unsupported),
        -3 => Err(PlatformError::Permission),
        -5 => Err(PlatformError::GeometryChanged),
        _ => Err(PlatformError::Unavailable),
    }
}

/// Whether this process may post input events (the Accessibility grant of
/// its responsible process). Checks only; never prompts.
pub fn access() -> bool {
    // SAFETY: a scalar query with no arguments.
    unsafe { fr_in_access() != 0 }
}

/// End this process when `expected` (its parent) exits, releasing whatever
/// the executor recorded as held first. Refuses a process already reparented.
pub fn bind_parent(expected: u32) -> Result<(), PlatformError> {
    let expected = i32::try_from(expected).map_err(|_| PlatformError::Unsupported)?;
    // SAFETY: a scalar argument; the glue installs one process-exit watch.
    status(unsafe { fr_in_bind_parent(expected) })
}

/// Post every later event of this process (owners, cleanup and the last-resort
/// release alike) to process `pid` only, never to the desktop. The event
/// test reads back what this executor posts this way, without touching the
/// owner's cursor or focus. The agent never calls it. A pid that is not a
/// process id is refused rather than leaving events on the desktop.
pub fn route_to_process(pid: u32) -> Result<(), PlatformError> {
    let pid = i32::try_from(pid)
        .ok()
        .filter(|&p| p > 0)
        .ok_or(PlatformError::Unsupported)?;
    // SAFETY: a scalar argument stored atomically by the glue.
    unsafe { fr_in_route_to_process(pid) };
    Ok(())
}

fn displays() -> Result<Vec<RawInputDisplay>, PlatformError> {
    let mut raw = [RawInputDisplay::default(); 16];
    let mut count = 0;
    // SAFETY: the buffer and count are writable for the stated capacity.
    status(unsafe { fr_in_displays(raw.as_mut_ptr(), raw.len(), &raw mut count) })?;
    Ok(raw[..count.min(raw.len())].to_vec())
}

const fn mac_display(raw: &RawInputDisplay) -> MacDisplay {
    let d = raw.display;
    MacDisplay {
        id: d.display_id,
        x: d.x,
        y: d.y,
        pixel_width: d.pixel_width,
        pixel_height: d.pixel_height,
        point_width: d.point_width,
        point_height: d.point_height,
    }
}

/// The grant's coordinates are the capture catalog's, which uses the absolute
/// protocol limits; a different bound would publish a different grid.
fn desktop_bounds(raw: &RawInputDisplay) -> Option<InputBounds> {
    mac_display(raw).desktop_bounds(&ProtocolLimits::ABSOLUTE)
}

/// Every active display a grant may name: its Core Graphics id and its
/// rectangle in the capture catalog's desktop coordinates.
pub fn display_bounds() -> Result<Vec<(u32, InputBounds)>, PlatformError> {
    Ok(displays()?
        .iter()
        .filter_map(|d| desktop_bounds(d).map(|b| (d.display.display_id, b)))
        .collect())
}

/// What this executor implements. Relative motion and text remain unsupported.
fn implemented() -> Capabilities {
    Capabilities::default()
        .with(Capability::Keys)
        .with(Capability::Repeat)
        .with(Capability::Absolute)
        .with(Capability::Buttons)
        .with(Capability::LineScroll)
        .with(Capability::PixelScroll)
}

/// frd's startup probe: the capabilities a lease executor would advertise,
/// with no event source, indicator or input. `Permission` without the
/// Accessibility grant, `Unavailable` without a representable display.
pub fn probe() -> Result<Capabilities, PlatformError> {
    if !access() {
        return Err(PlatformError::Permission);
    }
    if displays()?.iter().any(|d| desktop_bounds(d).is_some()) {
        Ok(implemented())
    } else {
        Err(PlatformError::Unavailable)
    }
}

/// Publish the executor's current pessimistic held set for the last-resort
/// release (a panic or the parent's exit). Lock-free.
pub fn record(held: Held) {
    let (low, high, buttons) = held.words();
    // SAFETY: scalar arguments stored atomically by the glue.
    unsafe { fr_in_record(low, high, buttons) }
}

/// Exit status after a panic released the recorded held set.
pub const EXIT_PANIC: i32 = 70;

/// Install the panic path: release the recorded set, then exit. Install once,
/// before any input; a panic never continues an executor.
pub fn install_panic_release() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // SAFETY: no arguments; the glue releases at most once per process.
        unsafe { fr_in_emergency_release() };
        previous(info);
        std::process::exit(EXIT_PANIC);
    }));
}

/// One lease's Quartz input owner, bound to one display.
pub struct MacPointer {
    source: NonNull<c_void>,
    display: RawInputDisplay,
    bounds: InputBounds,
    held: Held,
    clicks: Clicks,
    /// Where this owner last placed the pointer, in global points. Presses
    /// and scrolls go there: a just-posted move may still be in flight.
    position: Option<(f64, f64)>,
    prepared: Option<Operation>,
}

// SAFETY: the event source is a Core Foundation object usable from any thread;
// this owner is moved once to the executor thread and used only there.
unsafe impl Send for MacPointer {}

impl MacPointer {
    /// Bind the one active display whose desktop rectangle is exactly
    /// `bounds`, after checking the Accessibility grant and that every
    /// `required` capability is implemented. Nothing is posted.
    pub fn open_exact(bounds: InputBounds, required: Capabilities) -> Result<Self, PlatformError> {
        if !access() {
            return Err(PlatformError::Permission);
        }
        if !implemented().contains_all(required) {
            return Err(PlatformError::Unsupported);
        }
        let display = displays()?
            .into_iter()
            .find(|d| desktop_bounds(d) == Some(bounds))
            .ok_or(PlatformError::GeometryChanged)?;
        // SAFETY: no arguments; the returned source is owned and released once.
        let source = NonNull::new(unsafe { fr_in_open() }).ok_or(PlatformError::Unavailable)?;
        // SAFETY: a scalar query of the user's setting.
        let interval = unsafe { fr_in_double_click_interval() };
        let interval = Duration::try_from_secs_f64(interval.clamp(0.1, 5.0))
            .unwrap_or(Duration::from_millis(500));
        Ok(Self {
            source,
            display,
            bounds,
            held: Held::default(),
            clicks: Clicks::new(interval),
            position: None,
            prepared: None,
        })
    }

    pub fn capabilities(&self) -> Capabilities {
        implemented()
    }
    /// Quartz has a standalone autorepeat key event.
    pub const fn repeat_requires_pair(&self) -> bool {
        false
    }
    /// Quartz has a standalone line scroll event.
    pub const fn line_scroll_requires_pairs(&self) -> bool {
        false
    }
    /// The bound display, for placing the control indicator on it.
    pub const fn display_id(&self) -> u32 {
        self.display.display.display_id
    }

    /// Every key and button this owner holds OR has prepared to press: the
    /// last-resort release set. Never evidence that a press happened.
    pub fn native_held(&self) -> Held {
        let mut pressing = Held::default();
        match self.prepared {
            Some(Operation::Key {
                key,
                transition: KeyTransition::Press,
            }) => {
                if let Some(code) = macos_keys::keycode(key.usage()) {
                    pressing.set_key(code, true);
                }
            }
            Some(Operation::Button {
                button,
                pressed: true,
            }) => pressing.set_button(button_index(button), true),
            _ => {}
        }
        self.held.union(pressing)
    }

    fn validate_target(&self) -> Result<(), PlatformError> {
        // SAFETY: a readable description for the duration of the call.
        status(unsafe { fr_in_display_check(&raw const self.display) })
    }

    fn to_points(&self, point: DesktopPoint) -> (f64, f64) {
        let d = &self.display;
        macos_display::to_points(
            self.bounds,
            (d.origin_x, d.origin_y),
            (d.display.point_width, d.display.point_height),
            point,
        )
    }

    /// Where a press, release or scroll acts: this owner's last position, or
    /// the current pointer when it has not placed one. A positive operation
    /// outside the bound display is refused, never moved onto it.
    fn action_point(&self) -> Result<(f64, f64), PlatformError> {
        if let Some(position) = self.position {
            return Ok(position);
        }
        let (mut x, mut y) = (0.0, 0.0);
        // SAFETY: two writable doubles for the duration of the call.
        status(unsafe { fr_in_location(&raw mut x, &raw mut y) })?;
        let d = &self.display;
        let inside = x >= d.origin_x
            && y >= d.origin_y
            && x < d.origin_x + f64::from(d.display.point_width)
            && y < d.origin_y + f64::from(d.display.point_height);
        if inside {
            Ok((x, y))
        } else {
            Err(PlatformError::GeometryChanged)
        }
    }

    fn prepare_native(&self, op: Operation) -> Result<(), PlatformError> {
        if !op.is_release() {
            self.validate_target()?;
        }
        match op {
            Operation::Key { key, transition } => {
                let code = macos_keys::keycode(key.usage()).ok_or(PlatformError::Unsupported)?;
                let held = self.held.key(code);
                match transition {
                    KeyTransition::Press if held => Err(PlatformError::Permission),
                    KeyTransition::Release | KeyTransition::Repeat if !held => {
                        Err(PlatformError::Unsupported)
                    }
                    // Modifiers change flags; they never autorepeat.
                    KeyTransition::Repeat if macos_keys::modifier(code).is_some() => {
                        Err(PlatformError::Unsupported)
                    }
                    _ => Ok(()),
                }
            }
            Operation::Absolute(point) if self.bounds.contains(point) => Ok(()),
            Operation::Absolute(_) => Err(PlatformError::GeometryChanged),
            Operation::Button { button, pressed } => {
                let held = self.held.button(button_index(button));
                if pressed && held {
                    return Err(PlatformError::Permission);
                }
                if !pressed && !held {
                    return Err(PlatformError::Unsupported);
                }
                if pressed {
                    self.action_point()?;
                }
                Ok(())
            }
            Operation::Scroll {
                x,
                y,
                unit: ScrollUnit::Lines,
            } => {
                scroll_lines(x, y).ok_or(PlatformError::Unsupported)?;
                self.action_point().map(drop)
            }
            Operation::Scroll {
                x,
                y,
                unit: ScrollUnit::Pixels,
            } => {
                scroll_pixels(x, y).ok_or(PlatformError::Unsupported)?;
                self.action_point().map(drop)
            }
            _ => Err(PlatformError::Unsupported),
        }
    }

    fn submit_native(&mut self, op: Operation) -> Submission {
        let source = self.source.as_ptr();
        let result = match op {
            Operation::Key { key, transition } => {
                let Some(code) = macos_keys::keycode(key.usage()) else {
                    return Submission::NotSubmitted(PlatformError::Unsupported);
                };
                let modifier = macos_keys::modifier(code).is_some();
                let mut after = self.held;
                match transition {
                    // Held from before the call: the release set stays pessimistic.
                    KeyTransition::Press => {
                        after.set_key(code, true);
                        self.held = after;
                    }
                    KeyTransition::Release => after.set_key(code, false),
                    KeyTransition::Repeat => {}
                }
                // A modifier's own event carries the state after it.
                let flags = after.flags();
                // SAFETY: the owned live source; scalar arguments.
                let result = status(unsafe {
                    fr_in_key(
                        source,
                        code,
                        c_int::from(transition != KeyTransition::Release),
                        c_int::from(transition == KeyTransition::Repeat),
                        c_int::from(modifier),
                        flags,
                    )
                });
                if result.is_ok() {
                    self.held = after;
                }
                result
            }
            Operation::Absolute(point) => {
                let (x, y) = self.to_points(point);
                let held = self
                    .held
                    .dragging()
                    .and_then(|i| c_int::try_from(i).ok())
                    .unwrap_or(-1);
                // SAFETY: the owned live source; scalar arguments.
                let result = status(unsafe { fr_in_move(source, x, y, held, self.held.flags()) });
                if result.is_ok() {
                    self.position = Some((x, y));
                }
                result
            }
            Operation::Button { button, pressed } => {
                let index = button_index(button);
                let (x, y) = match self.action_point() {
                    Ok(point) => point,
                    Err(error) => return Submission::NotSubmitted(error),
                };
                let clicks = if pressed {
                    self.held.set_button(index, true);
                    self.clicks.press(index, Instant::now(), (x, y))
                } else {
                    self.clicks.release(index)
                };
                let number = c_int::try_from(index).unwrap_or(0);
                // SAFETY: the owned live source; scalar arguments.
                let result = status(unsafe {
                    fr_in_button(
                        source,
                        number,
                        c_int::from(pressed),
                        clicks,
                        x,
                        y,
                        self.held.flags(),
                    )
                });
                if result.is_ok() && !pressed {
                    self.held.set_button(index, false);
                }
                result
            }
            Operation::Scroll { x, y, unit } => return self.submit_scroll(x, y, unit),
            _ => return Submission::NotSubmitted(PlatformError::Unsupported),
        };
        match result {
            Ok(()) => Submission::Submitted,
            // The glue refused before creating an event: nothing was posted.
            Err(PlatformError::Unsupported) => Submission::NotSubmitted(PlatformError::Unsupported),
            Err(_) => Submission::Unknown,
        }
    }

    fn submit_scroll(&self, x: i32, y: i32, unit: ScrollUnit) -> Submission {
        let delta = match unit {
            ScrollUnit::Lines => scroll_lines(x, y),
            ScrollUnit::Pixels => scroll_pixels(x, y),
        };
        let Some((vertical, horizontal)) = delta else {
            return Submission::NotSubmitted(PlatformError::Unsupported);
        };
        let (px, py) = match self.action_point() {
            Ok(point) => point,
            Err(error) => return Submission::NotSubmitted(error),
        };
        let source = self.source.as_ptr();
        // SAFETY: the owned live source; scalar arguments. The caller consumed
        // this operation's preparation and original final authority check.
        let result = status(unsafe {
            match unit {
                ScrollUnit::Lines => {
                    fr_in_scroll(source, vertical, horizontal, px, py, self.held.flags())
                }
                ScrollUnit::Pixels => {
                    fr_in_pixel_scroll(source, vertical, horizontal, px, py, self.held.flags())
                }
            }
        });
        match result {
            Ok(()) => Submission::Submitted,
            Err(PlatformError::Unsupported) => Submission::NotSubmitted(PlatformError::Unsupported),
            Err(_) => Submission::Unknown,
        }
    }

    /// Check the operation now and retain it for `submit_checked`.
    pub fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.prepared = None;
        self.prepare_native(op)?;
        self.prepared = Some(op);
        Ok(())
    }

    /// Revalidate a retained preparation (it crossed an IPC boundary), then
    /// let its authority owner decide immediately before the event is posted.
    /// `final_check` receives the pessimistic held set to record and must not
    /// post or retry anything. Failure consumes the preparation.
    pub fn submit_checked<E>(
        &mut self,
        op: Operation,
        final_check: impl FnOnce(Held) -> Result<(), E>,
    ) -> Result<Submission, E> {
        if self.prepared != Some(op) {
            self.prepared = None;
            return Ok(Submission::NotSubmitted(PlatformError::Unsupported));
        }
        if let Err(error) = self.prepare_native(op) {
            self.prepared = None;
            return Ok(Submission::NotSubmitted(error));
        }
        if let Err(error) = final_check(self.native_held()) {
            self.prepared = None;
            return Err(error);
        }
        self.prepared = None;
        Ok(self.submit_native(op))
    }

    /// Submit a retained release without a further authority decision:
    /// release-only cleanup, which stays possible after revocation.
    pub fn submit(&mut self, op: Operation) -> Submission {
        match self.submit_checked(op, |_| Ok::<(), core::convert::Infallible>(())) {
            Ok(result) => result,
            Err(never) => match never {},
        }
    }

    pub fn cancel_prepared(&mut self) {
        self.prepared = None;
    }

    /// Release every key and button this owner holds. True when nothing is
    /// left held; false means native release remains uncertain.
    pub fn cleanup_native(&mut self) -> bool {
        self.prepared = None;
        let source = self.source.as_ptr();
        let keys: Vec<u16> = self.held.keys().collect();
        for code in keys {
            let mut after = self.held;
            after.set_key(code, false);
            let modifier = c_int::from(macos_keys::modifier(code).is_some());
            // SAFETY: the owned live source; scalar arguments.
            let posted = unsafe { fr_in_key(source, code, 0, 0, modifier, after.flags()) };
            if posted == 0 {
                self.held = after;
            }
        }
        let buttons: Vec<usize> = self.held.buttons().collect();
        for index in buttons {
            let Ok((x, y)) = self.position.map_or_else(
                || {
                    let (mut x, mut y) = (0.0, 0.0);
                    // SAFETY: two writable doubles for the duration of the call.
                    status(unsafe { fr_in_location(&raw mut x, &raw mut y) }).map(|()| (x, y))
                },
                Ok,
            ) else {
                continue;
            };
            let number = c_int::try_from(index).unwrap_or(0);
            let clicks = self.clicks.release(index);
            // SAFETY: the owned live source; scalar arguments.
            let posted =
                unsafe { fr_in_button(source, number, 0, clicks, x, y, self.held.flags()) };
            if posted == 0 {
                self.held.set_button(index, false);
            }
        }
        self.held.is_empty()
    }
}

impl Drop for MacPointer {
    fn drop(&mut self) {
        let _ = self.cleanup_native();
        // What is still held after the release, so a later last-resort
        // release (the parent's exit) does not post its key-ups again.
        record(self.held);
        // SAFETY: the source came from fr_in_open and is released exactly once.
        unsafe { fr_in_close(self.source.as_ptr()) };
    }
}

/// What the control indicator reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndicatorStatus {
    Shown,
    /// The local user clicked it (or hid it): a local revoke.
    LocalStop,
    /// It could not be kept shown: a native failure, not a revoke.
    Failed,
}

/// The always-on-top "remote control active" panel on the controlled display,
/// shown for the whole lease. `AppKit`: create, pump and drop it on the main
/// thread only (it is neither Send nor Sync).
pub struct Indicator {
    _main_thread: PhantomData<*const ()>,
}

/// The window server must show the panel within this bound.
const INDICATOR_TIMEOUT_MS: u32 = 2_000;

impl Indicator {
    /// Show the indicator on `display_id`; `Ok` only once it is on screen.
    pub fn open(display_id: u32) -> Result<Self, PlatformError> {
        // SAFETY: scalar arguments; the glue refuses any thread but the main one.
        status(unsafe { fr_in_indicator_open(display_id, INDICATOR_TIMEOUT_MS) })?;
        Ok(Self {
            _main_thread: PhantomData,
        })
    }
    /// Handle its events for up to `timeout`.
    pub fn pump(&self, timeout: Duration) -> IndicatorStatus {
        let ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
        // SAFETY: a scalar argument on the thread that opened the indicator.
        match unsafe { fr_in_indicator_pump(ms) } {
            0 => IndicatorStatus::Shown,
            1 => IndicatorStatus::LocalStop,
            _ => IndicatorStatus::Failed,
        }
    }
}

impl Drop for Indicator {
    fn drop(&mut self) {
        // SAFETY: the thread that opened it; the glue closes it once.
        unsafe { fr_in_indicator_close() };
    }
}
