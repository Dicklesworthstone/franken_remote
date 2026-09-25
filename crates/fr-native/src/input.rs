//! Explicit local-X11 keyboard/pointer adapter for the interactive input process.
//! Never load this in the broker/media worker. Xlib may block or terminate on
//! display loss; the independent watchdog/revoke path must remain outside it.
//! This is not a Wayland permission fallback or an X11 security sandbox.
use crate::keyboard::Keyboard;
pub mod emergency;
mod scroll;
use core::{
    ffi::{c_char, c_int, c_uint, c_ulong, c_void},
    marker::PhantomData,
    ptr::NonNull,
};
use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, PointerButton},
    input_submission::{Capabilities, Capability, InputSink, Operation, PlatformError, Submission},
};
use scroll::WheelState;
use std::{
    ffi::CString,
    rc::Rc,
    sync::{Mutex, MutexGuard},
};

// libXtst/libXext keep a process-global extension-display cache, including an
// unlocked last-display fast path. XInitThreads does not make cache lifetime
// safe against concurrent XTest lookup and XCloseDisplay. Serialize our foreign
// extension access, NOT authority. Acquire during preflight, before the final
// ticket/clock check; never wait for this lock for the first time in submit.
static XTEST: Mutex<()> = Mutex::new(());
fn xtest_access() -> MutexGuard<'static, ()> {
    XTEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultScreen(display: *mut c_void) -> c_int;
    fn XScreenCount(display: *mut c_void) -> c_int;
    fn XRootWindow(display: *mut c_void, screen: c_int) -> c_ulong;
    fn XGetGeometry(
        display: *mut c_void,
        drawable: c_ulong,
        root: *mut c_ulong,
        x: *mut c_int,
        y: *mut c_int,
        width: *mut c_uint,
        height: *mut c_uint,
        border: *mut c_uint,
        depth: *mut c_uint,
    ) -> c_int;
    fn XQueryPointer(
        display: *mut c_void,
        window: c_ulong,
        root: *mut c_ulong,
        child: *mut c_ulong,
        root_x: *mut c_int,
        root_y: *mut c_int,
        win_x: *mut c_int,
        win_y: *mut c_int,
        mask: *mut c_uint,
    ) -> c_int;
    fn XFlush(display: *mut c_void) -> c_int;
    fn XSync(display: *mut c_void, discard: c_int) -> c_int;
    fn XGetPointerMapping(display: *mut c_void, map: *mut u8, count: c_int) -> c_int;
}
// Linux's versioned ABI. No runtime loader search or downloaded library is used.
#[link(name = "libXtst.so.6", kind = "dylib", modifiers = "+verbatim")]
unsafe extern "C" {
    fn XTestQueryExtension(
        display: *mut c_void,
        event: *mut c_int,
        error: *mut c_int,
        major: *mut c_int,
        minor: *mut c_int,
    ) -> c_int;
    fn XTestFakeMotionEvent(
        display: *mut c_void,
        screen: c_int,
        x: c_int,
        y: c_int,
        delay: c_ulong,
    ) -> c_int;
    fn XTestFakeRelativeMotionEvent(
        display: *mut c_void,
        x: c_int,
        y: c_int,
        delay: c_ulong,
    ) -> c_int;
    fn XTestFakeButtonEvent(
        display: *mut c_void,
        button: c_uint,
        pressed: c_int,
        delay: c_ulong,
    ) -> c_int;
}

/// Thread-confined X11 connection. It owns no ticket, lease or network socket.
/// `InputSession` is the only production caller of its sink operations.
pub struct X11Pointer {
    display: NonNull<c_void>,
    screen: c_int,
    root: c_ulong,
    dimensions: (u32, u32),
    relative: bool,
    line_scroll: bool,
    wheel: WheelState,
    prepared: Option<Operation>,
    xtest: Option<MutexGuard<'static, ()>>,
    keyboard: Keyboard,
    buttons: [Option<u8>; 5],
    prepared_button: Option<u8>,
    _thread: PhantomData<Rc<()>>,
}
impl X11Pointer {
    /// Only a locally selected `:display[.screen]` is accepted. Never accept this
    /// selector from a remote peer or silently substitute Xwayland after refusal.
    pub fn open(name: &str) -> Result<Self, PlatformError> {
        if !local_display(name) {
            return Err(PlatformError::Unsupported);
        }
        crate::xlib::initialize_threads().map_err(|_| PlatformError::Unavailable)?;
        let name = CString::new(name).map_err(|_| PlatformError::Unsupported)?;
        // SAFETY: NUL-terminated name lives through call; returned context is
        // uniquely owned and used only on its creating thread until Drop.
        let display = NonNull::new(unsafe { XOpenDisplay(name.as_ptr()) })
            .ok_or(PlatformError::Unavailable)?;
        // SAFETY: live owned connection; these queries retain no caller pointer.
        let screen = unsafe { XDefaultScreen(display.as_ptr()) };
        let root = unsafe { XRootWindow(display.as_ptr(), screen) };
        let mut owner = Self {
            display,
            screen,
            root,
            dimensions: (0, 0),
            relative: false,
            line_scroll: false,
            wheel: WheelState::default(),
            prepared: None,
            xtest: None,
            keyboard: Keyboard::new(display),
            buttons: [None; 5],
            prepared_button: None,
            _thread: PhantomData,
        };
        let (mut event, mut error, mut major, mut minor) = (0, 0, 0, 0);
        // SAFETY: valid exclusively borrowed out parameters and live display.
        let available = {
            let _access = xtest_access();
            // SAFETY: the guard also excludes extension-display removal.
            unsafe {
                XTestQueryExtension(
                    owner.display.as_ptr(),
                    &raw mut event,
                    &raw mut error,
                    &raw mut major,
                    &raw mut minor,
                )
            }
        };
        if available == 0 || major < 2 {
            return Err(PlatformError::Unsupported);
        }
        // The relative XTest entry uses the CURRENT root, not a screen argument.
        // Multiple X screens could retarget input after preparation, so do not
        // advertise this path there. Screen count is fixed for the connection.
        // SAFETY: live owned display; this reads its connection setup metadata.
        owner.relative = relative_supported(major, minor, unsafe {
            XScreenCount(owner.display.as_ptr())
        });
        owner.line_scroll = owner.relative && owner.wheel_mapping_available();
        owner.dimensions = owner.geometry()?;
        if owner.dimensions.0 > 8192 || owner.dimensions.1 > 8192 {
            return Err(PlatformError::Unsupported);
        }
        Ok(owner)
    }
    /// Open the locally selected display and revalidate the exact granted
    /// bounds and negotiated capabilities before any input is possible. The
    /// single check shared by the in-process factory and the input executor.
    pub fn open_exact(
        name: &str,
        expected_bounds: InputBounds,
        required: Capabilities,
    ) -> Result<Self, PlatformError> {
        let sink = Self::open(name)?;
        if sink.bounds() != expected_bounds {
            return Err(PlatformError::GeometryChanged);
        }
        if !sink.capabilities().contains_all(required) {
            return Err(PlatformError::Unsupported);
        }
        Ok(sink)
    }
    /// Every native code this owner holds OR has prepared to press, including
    /// wheel buttons: the executor's last-resort release set. Pessimistic by
    /// design (an `XTest` release of an up key/button is ignored by the server);
    /// never evidence that a press happened.
    pub fn native_held(&self) -> NativeHeld {
        let mut held = NativeHeld::default();
        for code in self.keyboard.held_codes() {
            held.keys.insert(code);
        }
        for code in self
            .buttons
            .iter()
            .flatten()
            .chain(&self.prepared_button)
            .chain(self.wheel.held.as_ref().map(|(_, code)| code))
            .chain(
                self.wheel
                    .prepared
                    .as_ref()
                    .filter(|(_, pressed, _)| *pressed)
                    .map(|(_, _, code)| code),
            )
        {
            held.buttons.insert(*code);
        }
        held
    }
    pub fn capabilities(&self) -> Capabilities {
        let mut caps = Capabilities::default()
            .with(Capability::Absolute)
            .with(Capability::Buttons);
        if self.relative {
            caps = caps.with(Capability::Relative);
        }
        if self.line_scroll {
            caps = caps.with(Capability::LineScroll);
        }
        if self.keyboard.enabled() {
            caps.with(Capability::Keys).with(Capability::Repeat)
        } else {
            caps
        }
    }
    /// Retire input authority before this local-only keyboard cleanup. Returns
    /// false when native release/restoration is still uncertain; do not hand off.
    pub fn cleanup_keyboard(&mut self) -> bool {
        self.cancel_prepared();
        let _access = xtest_access();
        self.keyboard.cleanup()
    }
    /// Local release-only teardown after the input lease has been retired.
    /// Both keyboard preparation and this owner's recorded button presses are
    /// cleaned up. False means native cleanup remains pending; retry locally,
    /// never grant another controller on the strength of core held-count alone.
    pub fn cleanup_native(&mut self) -> bool {
        self.cancel_prepared();
        let _access = xtest_access();
        let keyboard_done = self.keyboard.cleanup();
        let wheel_done = self.cleanup_wheel();
        for index in 0..self.buttons.len() {
            let Some(code) = self.buttons[index] else {
                continue;
            };
            // SAFETY: release only the physical code recorded BEFORE our press.
            // XSync orders server processing; it is not an application receipt.
            let accepted = unsafe {
                let accepted = XTestFakeButtonEvent(self.display.as_ptr(), u32::from(code), 0, 0);
                XSync(self.display.as_ptr(), 0);
                accepted
            };
            if accepted == 0 {
                continue;
            }
            let logical = [1u32, 3, 2, 8, 9][index];
            // Core XQueryPointer exposes only the first five logical buttons.
            // Extended buttons have API/server-sync evidence, not a held mask.
            if logical <= 5
                && !self
                    .query_pointer()
                    .is_ok_and(|(_, mask)| mask & (1 << (7 + logical)) == 0)
            {
                continue;
            }
            self.buttons[index] = None;
        }
        keyboard_done && wheel_done && self.buttons.iter().all(Option::is_none)
    }
    fn prepare_button(
        &mut self,
        button: PointerButton,
        pressed: bool,
    ) -> Result<(), PlatformError> {
        let index = button as usize - 1;
        if !pressed {
            self.prepared_button = Some(self.buttons[index].ok_or(PlatformError::Unsupported)?);
            return Ok(());
        }
        if self.geometry()? != self.dimensions {
            return Err(PlatformError::GeometryChanged);
        }
        if self.buttons[index].is_some() {
            return Err(PlatformError::Permission);
        }
        let logical = match button {
            PointerButton::Primary => 1u8,
            PointerButton::Secondary => 3,
            PointerButton::Middle => 2,
            PointerButton::Back => 8,
            PointerButton::Forward => 9,
        };
        if logical <= 5 && self.query_pointer()?.1 & (1 << (7 + logical)) != 0 {
            return Err(PlatformError::Permission);
        }
        let mut mapping = [0u8; 256];
        // SAFETY: live display and fixed-size map. Check returned count before
        // indexing; zero disables a physical button and duplicates are refused.
        let count = unsafe { XGetPointerMapping(self.display.as_ptr(), mapping.as_mut_ptr(), 256) };
        let count = usize::try_from(count)
            .ok()
            .filter(|n| (1..=256).contains(n))
            .ok_or(PlatformError::Unavailable)?;
        let mut selected = None;
        for (index, mapped) in mapping[..count].iter().enumerate() {
            if *mapped == logical {
                if selected.is_some() {
                    return Err(PlatformError::Unsupported);
                }
                selected = Some(u8::try_from(index + 1).map_err(|_| PlatformError::Unsupported)?);
            }
        }
        let code = selected.ok_or(PlatformError::Unsupported)?;
        if self.buttons.contains(&Some(code)) {
            return Err(PlatformError::Permission);
        }
        self.prepared_button = Some(code);
        Ok(())
    }
    pub fn bounds(&self) -> InputBounds {
        InputBounds::new(
            DesktopPoint { x: 0, y: 0 },
            self.dimensions.0,
            self.dimensions.1,
        )
        .expect("validated root geometry")
    }
    fn geometry(&self) -> Result<(u32, u32), PlatformError> {
        let (mut root, mut x, mut y, mut w, mut h, mut border, mut depth) = (0, 0, 0, 0, 0, 0, 0);
        // SAFETY: root belongs to this live display. Each out pointer addresses
        // independent initialized stack storage with the exact C ABI type.
        let ok = unsafe {
            XGetGeometry(
                self.display.as_ptr(),
                self.root,
                &raw mut root,
                &raw mut x,
                &raw mut y,
                &raw mut w,
                &raw mut h,
                &raw mut border,
                &raw mut depth,
            )
        };
        if ok == 0 || w == 0 || h == 0 {
            Err(PlatformError::Unavailable)
        } else {
            Ok((w, h))
        }
    }
    /// Explicit local instrumentation, not automatic telemetry. The returned
    /// button mask is X11-observed state, not ownership of physical input.
    pub fn query_pointer(&mut self) -> Result<(DesktopPoint, u32), PlatformError> {
        let (mut root, mut child, mut x, mut y, mut wx, mut wy, mut mask) = (0, 0, 0, 0, 0, 0, 0);
        // SAFETY: live owned connection and correctly sized independent outputs.
        if unsafe {
            XQueryPointer(
                self.display.as_ptr(),
                self.root,
                &raw mut root,
                &raw mut child,
                &raw mut x,
                &raw mut y,
                &raw mut wx,
                &raw mut wy,
                &raw mut mask,
            )
        } == 0
        {
            return Err(PlatformError::Unavailable);
        }
        Ok((DesktopPoint { x, y }, mask))
    }
}
impl InputSink for X11Pointer {
    fn prepare(&mut self, op: Operation) -> Result<(), PlatformError> {
        self.cancel_prepared();
        match op {
            Operation::Key { key, transition } => {
                if transition != KeyTransition::Release && self.geometry()? != self.dimensions {
                    return Err(PlatformError::GeometryChanged);
                }
                self.keyboard.prepare(key, transition)?;
            }
            Operation::Absolute(position) => {
                if !self.bounds().contains(position) {
                    return Err(PlatformError::GeometryChanged);
                }
                if self.geometry()? != self.dimensions {
                    return Err(PlatformError::GeometryChanged);
                }
            }
            Operation::Relative { x, y } => {
                if !self.relative || i16::try_from(x).is_err() || i16::try_from(y).is_err() {
                    return Err(PlatformError::Unsupported);
                }
                if self.geometry()? != self.dimensions {
                    return Err(PlatformError::GeometryChanged);
                }
                // Refuse a known off-display target instead of relying on XTest
                // to clamp it. Keep the operation relative: a local movement
                // after this query must not be overwritten by a stale absolute
                // position. The native receipt remains API-submitted evidence.
                let current = self.query_pointer()?.0;
                let target = current.x.checked_add(x).zip(current.y.checked_add(y));
                if !target.is_some_and(|(x, y)| self.bounds().contains(DesktopPoint { x, y })) {
                    return Err(PlatformError::GeometryChanged);
                }
            }
            Operation::Wheel { direction, pressed } => self.prepare_wheel(direction, pressed)?,
            // Release-only cleanup still works after geometry replacement.
            Operation::Button { button, pressed } => self.prepare_button(button, pressed)?,
            _ => return Err(PlatformError::Unsupported),
        }
        // Wait for foreign-cache exclusivity only after native queries. The
        // caller still samples authority AFTER this returns. A hung preflight
        // on one display must not hold the shared extension cache lock.
        let access = xtest_access();
        self.prepared = Some(op);
        self.xtest = Some(access);
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        let access = self.xtest.take();
        if self.prepared.take() != Some(op) || access.is_none() {
            return Submission::NotSubmitted(PlatformError::Unsupported);
        }
        if let Operation::Key { key, transition } = op {
            return self.keyboard.submit(key, transition);
        }
        // SAFETY: validated operation and owned display. Delay=0 prevents a
        // server-side scheduled replay. No Rust pointer is retained. XFlush
        // submits this bounded request before return, not an application ACK.
        let accepted = unsafe {
            match op {
                Operation::Absolute(p) => {
                    XTestFakeMotionEvent(self.display.as_ptr(), self.screen, p.x, p.y, 0)
                }
                Operation::Wheel { direction, pressed } => self.submit_wheel(direction, pressed),
                Operation::Relative { x, y } => {
                    // One checked signed-16-bit displacement, no splitting,
                    // acceleration emulation, delayed replay or absolute warp.
                    XTestFakeRelativeMotionEvent(self.display.as_ptr(), x, y, 0)
                }
                Operation::Button { button, pressed } => {
                    let Some(code) = self.prepared_button.take() else {
                        return Submission::NotSubmitted(PlatformError::Unsupported);
                    };
                    if pressed {
                        self.buttons[button as usize - 1] = Some(code);
                    }
                    XTestFakeButtonEvent(
                        self.display.as_ptr(),
                        u32::from(code),
                        c_int::from(pressed),
                        0,
                    )
                }
                _ => return Submission::NotSubmitted(PlatformError::Unsupported),
            }
        };
        if accepted == 0 {
            return Submission::Unknown;
        }
        // SAFETY: live connection. Fatal Xlib I/O errors remain process failures;
        // never catch one and pretend the server rolled back an input event.
        unsafe {
            XFlush(self.display.as_ptr());
        }
        if let Operation::Button {
            button,
            pressed: false,
        } = op
        {
            self.buttons[button as usize - 1] = None;
        }
        Submission::Submitted
    }
    fn cancel_prepared(&mut self) {
        self.prepared = None;
        self.prepared_button = None;
        self.wheel.prepared = None;
        self.keyboard.cancel_prepared();
        self.xtest = None;
    }
    fn repeat_requires_pair(&self) -> bool {
        true
    }
    fn line_scroll_requires_pairs(&self) -> bool {
        true
    }
}
impl Drop for X11Pointer {
    fn drop(&mut self) {
        let _ = self.cleanup_native();
        let _access = xtest_access();
        // SAFETY: unique live context; serialize extension cache removal with
        // every other XTest lookup/registration/removal in this adapter.
        unsafe {
            XCloseDisplay(self.display.as_ptr());
        }
    }
}
/// A fixed 256-bit set of native X11 codes. No allocation, no Debug payload.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct CodeSet([u64; 4]);
impl CodeSet {
    pub fn insert(&mut self, code: u8) {
        self.0[usize::from(code / 64)] |= 1 << (code % 64);
    }
    pub fn contains(&self, code: u8) -> bool {
        self.0[usize::from(code / 64)] & (1 << (code % 64)) != 0
    }
    pub fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }
    pub fn codes(&self) -> impl Iterator<Item = u8> + '_ {
        (0..=u8::MAX).filter(|code| self.contains(*code))
    }
    pub const fn words(&self) -> [u64; 4] {
        self.0
    }
    pub const fn from_words(words: [u64; 4]) -> Self {
        Self(words)
    }
}
/// Keyboard keycodes and physical pointer-button codes an owner may hold.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeHeld {
    pub keys: CodeSet,
    pub buttons: CodeSet,
}
impl NativeHeld {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.buttons.is_empty()
    }
}
impl core::fmt::Debug for NativeHeld {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Which keys are held is input content; only emptiness is diagnostic.
        f.debug_struct("NativeHeld")
            .field("empty", &self.is_empty())
            .finish()
    }
}
fn relative_supported(major: c_int, minor: c_int, screens: c_int) -> bool {
    major == 2 && minor >= 1 && screens == 1
}
pub(crate) fn local_display(name: &str) -> bool {
    if name.len() > 32 {
        return false;
    }
    let Some(rest) = name.strip_prefix(':') else {
        return false;
    };
    let mut parts = rest.split('.');
    let valid = |s: &str| {
        !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) && s.parse::<u16>().is_ok()
    };
    valid(parts.next().unwrap_or("")) && parts.next().is_none_or(valid) && parts.next().is_none()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn relative_requires_qualified_xtest_version_and_one_root() {
        assert!(relative_supported(2, 1, 1));
        assert!(relative_supported(2, 2, 1));
        for (major, minor, screens) in [(1, 9, 1), (2, 0, 1), (3, 0, 1), (2, 2, 0), (2, 2, 2)] {
            assert!(!relative_supported(major, minor, screens));
        }
    }
    #[test]
    fn only_explicit_local_display_selectors_are_accepted() {
        for s in [":0", ":19.1"] {
            assert!(local_display(s));
        }
        for s in [
            "",
            ":",
            ":0.",
            ":0.1.2",
            "localhost:0",
            "host:0",
            ":0\0",
            ":-1",
            ":65536",
        ] {
            assert!(!local_display(s));
        }
    }
}
