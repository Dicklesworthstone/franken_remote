//! Explicit local-X11 pointer adapter for the interactive input process.
//! Never load this in the broker/media worker. Xlib may block or terminate on
//! display loss; the independent watchdog/revoke path must remain outside it.
//! This is not a Wayland permission fallback or an X11 security sandbox.
use core::{
    ffi::{c_char, c_int, c_uint, c_ulong, c_void},
    marker::PhantomData,
    ptr::NonNull,
};
use fr_core::{
    input::{DesktopPoint, InputBounds, PointerButton},
    input_submission::{Capabilities, Capability, InputSink, Operation, PlatformError, Submission},
};
use std::{ffi::CString, rc::Rc};

#[link(name = "X11")]
unsafe extern "C" {
    fn XOpenDisplay(name: *const c_char) -> *mut c_void;
    fn XCloseDisplay(display: *mut c_void) -> c_int;
    fn XDefaultScreen(display: *mut c_void) -> c_int;
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
    prepared: Option<Operation>,
    _thread: PhantomData<Rc<()>>,
}
impl X11Pointer {
    /// Only a locally selected `:display[.screen]` is accepted. Never accept this
    /// selector from a remote peer or silently substitute Xwayland after refusal.
    pub fn open(name: &str) -> Result<Self, PlatformError> {
        if !local_display(name) {
            return Err(PlatformError::Unsupported);
        }
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
            prepared: None,
            _thread: PhantomData,
        };
        let (mut event, mut error, mut major, mut minor) = (0, 0, 0, 0);
        // SAFETY: valid exclusively borrowed out parameters and live display.
        if unsafe {
            XTestQueryExtension(
                owner.display.as_ptr(),
                &raw mut event,
                &raw mut error,
                &raw mut major,
                &raw mut minor,
            )
        } == 0
            || major < 2
        {
            return Err(PlatformError::Unsupported);
        }
        owner.dimensions = owner.geometry()?;
        if owner.dimensions.0 > 8192 || owner.dimensions.1 > 8192 {
            return Err(PlatformError::Unsupported);
        }
        Ok(owner)
    }
    pub fn capabilities(&self) -> Capabilities {
        Capabilities::default()
            .with(Capability::Absolute)
            .with(Capability::Buttons)
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
        self.prepared = None;
        match op {
            Operation::Absolute(position) => {
                if !self.bounds().contains(position) {
                    return Err(PlatformError::GeometryChanged);
                }
                if self.geometry()? != self.dimensions {
                    return Err(PlatformError::GeometryChanged);
                }
            }
            Operation::Button { pressed: true, .. } => {
                if self.geometry()? != self.dimensions {
                    return Err(PlatformError::GeometryChanged);
                }
            }
            // Release-only cleanup must still work after geometry replacement.
            Operation::Button { pressed: false, .. } => {}
            _ => return Err(PlatformError::Unsupported),
        }
        self.prepared = Some(op);
        Ok(())
    }
    fn submit(&mut self, op: Operation) -> Submission {
        if self.prepared.take() != Some(op) {
            return Submission::NotSubmitted(PlatformError::Unsupported);
        }
        // SAFETY: validated operation and owned display. Delay=0 prevents a
        // server-side scheduled replay. No Rust pointer is retained. XFlush
        // submits this bounded request before return, not an application ACK.
        let accepted = unsafe {
            match op {
                Operation::Absolute(p) => {
                    XTestFakeMotionEvent(self.display.as_ptr(), self.screen, p.x, p.y, 0)
                }
                Operation::Button { button, pressed } => XTestFakeButtonEvent(
                    self.display.as_ptr(),
                    match button {
                        PointerButton::Primary => 1,
                        PointerButton::Secondary => 3,
                        PointerButton::Middle => 2,
                        PointerButton::Back => 8,
                        PointerButton::Forward => 9,
                    },
                    c_int::from(pressed),
                    0,
                ),
                _ => return Submission::NotSubmitted(PlatformError::Unsupported),
            }
        };
        if accepted == 0 {
            return Submission::NotSubmitted(PlatformError::Unavailable);
        }
        // SAFETY: live connection. Fatal Xlib I/O errors remain process failures;
        // never catch one and pretend the server rolled back an input event.
        unsafe {
            XFlush(self.display.as_ptr());
        }
        Submission::Submitted
    }
}
impl Drop for X11Pointer {
    fn drop(&mut self) {
        // SAFETY: unique live context, no other thread or callback holds it.
        unsafe {
            XCloseDisplay(self.display.as_ptr());
        }
    }
}
fn local_display(name: &str) -> bool {
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
