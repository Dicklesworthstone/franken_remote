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
    KEY_NAMES
        .iter()
        .find(|(usage, _)| *usage == key.usage())
        .map(|(_, name)| *name)
}
const KEY_NAMES: &[(u16, [u8; 4])] = &[
    (0x04, *b"AC01"),
    (0x05, *b"AB05"),
    (0x06, *b"AB03"),
    (0x07, *b"AC03"),
    (0x08, *b"AD03"),
    (0x09, *b"AC04"),
    (0x0a, *b"AC05"),
    (0x0b, *b"AC06"),
    (0x0c, *b"AD08"),
    (0x0d, *b"AC07"),
    (0x0e, *b"AC08"),
    (0x0f, *b"AC09"),
    (0x10, *b"AB07"),
    (0x11, *b"AB06"),
    (0x12, *b"AD09"),
    (0x13, *b"AD10"),
    (0x14, *b"AD01"),
    (0x15, *b"AD04"),
    (0x16, *b"AC02"),
    (0x17, *b"AD05"),
    (0x18, *b"AD07"),
    (0x19, *b"AB04"),
    (0x1a, *b"AD02"),
    (0x1b, *b"AB02"),
    (0x1c, *b"AD06"),
    (0x1d, *b"AB01"),
    (0x1e, *b"AE01"),
    (0x1f, *b"AE02"),
    (0x20, *b"AE03"),
    (0x21, *b"AE04"),
    (0x22, *b"AE05"),
    (0x23, *b"AE06"),
    (0x24, *b"AE07"),
    (0x25, *b"AE08"),
    (0x26, *b"AE09"),
    (0x27, *b"AE10"),
    (0x28, *b"RTRN"),
    (0x29, *b"ESC\0"),
    (0x2a, *b"BKSP"),
    (0x2b, *b"TAB\0"),
    (0x2c, *b"SPCE"),
    (0x2d, *b"AE11"),
    (0x2e, *b"AE12"),
    (0x2f, *b"AD11"),
    (0x30, *b"AD12"),
    (0x31, *b"BKSL"),
    (0x33, *b"AC10"),
    (0x34, *b"AC11"),
    (0x35, *b"TLDE"),
    (0x36, *b"AB08"),
    (0x37, *b"AB09"),
    (0x38, *b"AB10"),
    (0x39, *b"CAPS"),
    (0x3a, *b"FK01"),
    (0x3b, *b"FK02"),
    (0x3c, *b"FK03"),
    (0x3d, *b"FK04"),
    (0x3e, *b"FK05"),
    (0x3f, *b"FK06"),
    (0x40, *b"FK07"),
    (0x41, *b"FK08"),
    (0x42, *b"FK09"),
    (0x43, *b"FK10"),
    (0x44, *b"FK11"),
    (0x45, *b"FK12"),
    (0x46, *b"PRSC"),
    (0x47, *b"SCLK"),
    (0x48, *b"PAUS"),
    (0x49, *b"INS\0"),
    (0x4a, *b"HOME"),
    (0x4b, *b"PGUP"),
    (0x4c, *b"DELE"),
    (0x4d, *b"END\0"),
    (0x4e, *b"PGDN"),
    (0x4f, *b"RGHT"),
    (0x50, *b"LEFT"),
    (0x51, *b"DOWN"),
    (0x52, *b"UP\0\0"),
    (0x53, *b"NMLK"),
    (0x54, *b"KPDV"),
    (0x55, *b"KPMU"),
    (0x56, *b"KPSU"),
    (0x57, *b"KPAD"),
    (0x58, *b"KPEN"),
    (0x59, *b"KP1\0"),
    (0x5a, *b"KP2\0"),
    (0x5b, *b"KP3\0"),
    (0x5c, *b"KP4\0"),
    (0x5d, *b"KP5\0"),
    (0x5e, *b"KP6\0"),
    (0x5f, *b"KP7\0"),
    (0x60, *b"KP8\0"),
    (0x61, *b"KP9\0"),
    (0x62, *b"KP0\0"),
    (0x63, *b"KPDL"),
    (0x64, *b"LSGT"),
    (0x65, *b"MENU"),
    (0x67, *b"KPEQ"),
    (0x68, *b"FK13"),
    (0x69, *b"FK14"),
    (0x6a, *b"FK15"),
    (0x6b, *b"FK16"),
    (0x6c, *b"FK17"),
    (0x6d, *b"FK18"),
    (0x6e, *b"FK19"),
    (0x6f, *b"FK20"),
    (0x70, *b"FK21"),
    (0x71, *b"FK22"),
    (0x72, *b"FK23"),
    (0x73, *b"FK24"),
    (0xe0, *b"LCTL"),
    (0xe1, *b"LFSH"),
    (0xe2, *b"LALT"),
    (0xe3, *b"LWIN"),
    (0xe4, *b"RCTL"),
    (0xe5, *b"RTSH"),
    (0xe6, *b"RALT"),
    (0xe7, *b"RWIN"),
];
