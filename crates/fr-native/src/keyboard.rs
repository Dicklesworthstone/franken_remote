//! Physical-key support for the existing X11 input owner. No character/layout
//! guessing, no separate connection or authority state. XKB preflight completes
//! before `InputSession` samples its final authorization clock.
use core::{
    ffi::{c_int, c_uint, c_void},
    ptr::NonNull,
};
use fr_core::{
    input::{KeyTransition, PhysicalKey},
    input_submission::{PlatformError, Submission},
};

unsafe extern "C" {
    fn fr_key_probe(display: *mut c_void) -> c_int;
    fn fr_key_code(display: *mut c_void, name: *const u8) -> c_int;
    fn fr_key_down(display: *mut c_void, code: c_uint) -> c_int;
    fn fr_key_repeat(display: *mut c_void, code: c_uint, mode: c_int) -> c_int;
    fn fr_key_event(display: *mut c_void, code: c_uint, pressed: c_int) -> c_int;
}
#[derive(Clone, Copy)]
struct Held {
    code: u8,
    repeat: bool,
    down: bool,
}
pub(super) struct Keyboard {
    display: NonNull<c_void>,
    enabled: bool,
    held: [Option<Held>; 256],
    prepared: Option<(PhysicalKey, KeyTransition)>,
}
impl Keyboard {
    /// The enclosing owner keeps this display alive through cleanup and never
    /// moves it to another thread. This module neither closes nor duplicates it.
    pub(super) fn new(display: NonNull<c_void>) -> Self {
        // SAFETY: input owner supplies its live, thread-confined Xlib display.
        let enabled = unsafe { fr_key_probe(display.as_ptr()) != 0 };
        Self {
            display,
            enabled,
            held: [None; 256],
            prepared: None,
        }
    }
    pub(super) const fn enabled(&self) -> bool {
        self.enabled
    }
    /// X keycodes this owner holds or has prepared to press. Pessimistic by
    /// design: it is the last-resort release set, never evidence of a press.
    pub(super) fn held_codes(&self) -> impl Iterator<Item = u8> + '_ {
        self.held.iter().flatten().map(|h| h.code)
    }
    fn repeat(&self, code: u8, mode: c_int) -> Result<bool, PlatformError> {
        // SAFETY: bounded XKB keycode; mode is query=-1, disabled=0, enabled=1.
        match unsafe { fr_key_repeat(self.display.as_ptr(), u32::from(code), mode) } {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(PlatformError::Unavailable),
        }
    }
    fn restore(&mut self, index: usize) -> Result<(), PlatformError> {
        let Some(h) = self.held[index] else {
            return Ok(());
        };
        if h.down {
            return Err(PlatformError::Unavailable);
        }
        if self.repeat(h.code, -1)? != h.repeat
            && self.repeat(h.code, c_int::from(h.repeat))? != h.repeat
        {
            return Err(PlatformError::Unavailable);
        }
        self.held[index] = None;
        Ok(())
    }
    pub(super) fn prepare(
        &mut self,
        key: PhysicalKey,
        transition: KeyTransition,
    ) -> Result<(), PlatformError> {
        self.cancel_prepared();
        if !self.enabled || transition == KeyTransition::Repeat {
            return Err(PlatformError::Unsupported);
        }
        let i = usize::from(key.usage());
        if transition == KeyTransition::Release {
            if self.held[i].is_none() {
                return Err(PlatformError::Unsupported);
            }
        } else {
            // Failed restoration blocks a new press; do not overwrite its saved
            // state. Cleanup can retry, but admission must not guess it succeeded.
            if self.held.iter().flatten().any(|h| !h.down) {
                return Err(PlatformError::Unavailable);
            }
            if self.held[i].is_some() {
                return Err(PlatformError::Permission);
            }
            let name = key_name(key).ok_or(PlatformError::Unsupported)?;
            // SAFETY: exactly four bytes of a physical XKB name, borrowed only.
            let code = unsafe { fr_key_code(self.display.as_ptr(), name.as_ptr()) };
            let code = u8::try_from(code)
                .ok()
                .filter(|c| *c >= 8)
                .ok_or(PlatformError::Unsupported)?;
            if self.held.iter().flatten().any(|h| h.code == code) {
                return Err(PlatformError::Permission);
            }
            // SAFETY: live display and validated keycode. This is a query, not input.
            match unsafe { fr_key_down(self.display.as_ptr(), u32::from(code)) } {
                0 => {}
                1 => return Err(PlatformError::Permission),
                _ => return Err(PlatformError::Unavailable),
            }
            let repeat = self.repeat(code, -1)?;
            self.held[i] = Some(Held {
                code,
                repeat,
                down: false,
            });
            self.prepared = Some((key, transition));
            // Reversible preparation, never a press. The core guard restores it
            // when authority expires during this round trip or preparation unwinds.
            if self.repeat(code, 0)? {
                return Err(PlatformError::Unavailable);
            }
        }
        self.prepared = Some((key, transition));
        Ok(())
    }
    pub(super) fn submit(&mut self, key: PhysicalKey, transition: KeyTransition) -> Submission {
        if self.prepared.take() != Some((key, transition)) {
            return Submission::NotSubmitted(PlatformError::Unsupported);
        }
        let i = usize::from(key.usage());
        let Some(h) = self.held[i] else {
            return Submission::NotSubmitted(PlatformError::Unavailable);
        };
        let pressed = transition == KeyTransition::Press;
        if pressed {
            self.held[i].as_mut().expect("owned").down = true;
        }
        // SAFETY: original mapped code is retained for release even if the XKB
        // map changes while held. One XTest operation, with zero server delay.
        let accepted = unsafe {
            fr_key_event(
                self.display.as_ptr(),
                u32::from(h.code),
                c_int::from(pressed),
            )
        };
        if accepted == 0 {
            return Submission::Unknown;
        }
        if !pressed {
            self.held[i].as_mut().expect("owned").down = false;
            // Restoration is release-only cleanup. Failure is not permission to
            // claim full completion or forget the locally retained cleanup state.
            if self.restore(i).is_err() {
                return Submission::Unknown;
            }
        }
        Submission::Submitted
    }
    pub(super) fn cancel_prepared(&mut self) {
        if let Some((key, KeyTransition::Press)) = self.prepared.take() {
            let i = usize::from(key.usage());
            if self.held[i].is_some_and(|h| !h.down) {
                let _ = self.restore(i);
            }
        }
    }
    /// Local release-only cleanup, also used before the enclosing `XCloseDisplay`.
    /// False leaves restoration/release state retained for explicit retry.
    pub(super) fn cleanup(&mut self) -> bool {
        self.cancel_prepared();
        for i in 0..self.held.len() {
            if let Some(h) = self.held[i] {
                if h.down {
                    // SAFETY: only codes this owner recorded before a press.
                    if unsafe { fr_key_event(self.display.as_ptr(), u32::from(h.code), 0) } == 0 {
                        continue;
                    }
                    // SAFETY: confirm server release before forgetting held state.
                    if unsafe { fr_key_down(self.display.as_ptr(), u32::from(h.code)) } != 0 {
                        continue;
                    }
                    self.held[i].as_mut().expect("owned").down = false;
                }
                let _ = self.restore(i);
            }
        }
        self.held.iter().all(Option::is_none)
    }
}

/// USB HID physical positions to XKB names, never keysyms or evdev offsets.
fn key_name(key: PhysicalKey) -> Option<[u8; 4]> {
    crate::key_names::KEY_NAMES
        .iter()
        .find(|(usage, _)| *usage == key.usage())
        .map(|(_, name)| *name)
}
