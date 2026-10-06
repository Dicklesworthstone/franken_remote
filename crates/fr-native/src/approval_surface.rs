//! Thread-confined native approval surface used only by a supervised session
//! child. The existing C bridge owns device attribution and input exclusion.
//! This module has no admission, input lease, network, or terminal fallback.
use std::{
    ffi::{CString, c_char, c_int, c_void},
    marker::PhantomData,
    ptr::NonNull,
    rc::Rc,
    time::{Duration, Instant},
};

const TURN_EVENTS: usize = 32;
const MAP_TIMEOUT: Duration = Duration::from_secs(2);

unsafe extern "C" {
    fn fr_approval_open(display: *const c_char, role: u32, window: *mut u32) -> *mut c_void;
    fn fr_indicator_next(handle: *mut c_void, kind: *mut u32) -> c_int;
    fn fr_indicator_draw(handle: *mut c_void) -> c_int;
    fn fr_indicator_close(handle: *mut c_void);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Observe,
    RequestControl,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Opening,
    Mapped,
    Allowed,
    Denied,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidDisplay,
    Unavailable,
    Expired,
    Closed,
}

/// No `Send`/`Sync`: create, poll and destroy on the same native child thread.
/// A driver stall is contained by the parent's original process deadline.
pub struct Surface {
    raw: Option<NonNull<c_void>>,
    opened: Instant,
    until: Instant,
    mapped: bool,
    drawn: bool,
    _thread: PhantomData<Rc<()>>,
}
impl Surface {
    pub fn open(display: &str, role: Role, until: Instant) -> Result<Self, Error> {
        let opened = Instant::now();
        if opened >= until {
            return Err(Error::Expired);
        }
        let Some(number) = display.strip_prefix(':') else {
            return Err(Error::InvalidDisplay);
        };
        let mut parts = number.split('.');
        let valid = |part: &str| {
            !part.is_empty() && part.len() <= 5 && part.bytes().all(|b| b.is_ascii_digit())
                && part.parse::<u16>().is_ok()
        };
        if !parts.next().is_some_and(valid)
            || !parts.next().is_none_or(valid)
            || parts.next().is_some()
        {
            return Err(Error::InvalidDisplay);
        }
        let display = CString::new(display).map_err(|_| Error::InvalidDisplay)?;
        let role = match role { Role::Observe => 0, Role::RequestControl => 1 };
        let mut window = 0;
        // SAFETY: local NUL-terminated display and writable scalar outlive the
        // call. C retains neither pointer and creates a uniquely owned handle.
        let raw = unsafe { fr_approval_open(display.as_ptr(), role, &raw mut window) };
        let raw = NonNull::new(raw).ok_or(Error::Unavailable)?;
        let mut surface = Self {
            raw: Some(raw), opened, until, mapped: false, drawn: false, _thread: PhantomData,
        };
        if window == 0 {
            surface.close();
            return Err(Error::Unavailable);
        }
        if Instant::now() >= until {
            surface.close();
            return Err(Error::Expired);
        }
        Ok(surface)
    }

    /// At most 32 native events, never an unbounded drain. Only the hardened
    /// bridge's device-attributed Allow event can produce a positive result.
    /// Every terminal path retires the actual UI before returning its decision.
    pub fn poll(&mut self) -> Result<State, Error> {
        let result = self.turn();
        if matches!(result, Ok(State::Allowed | State::Denied) | Err(_)) {
            self.close();
            // Native destruction may itself block; it cannot renew consent.
            if matches!(result, Ok(State::Allowed)) && Instant::now() >= self.until {
                return Err(Error::Expired);
            }
        }
        result
    }
    fn turn(&mut self) -> Result<State, Error> {
        let raw = self.raw.ok_or(Error::Closed)?;
        let mut draw = false;
        let mut allowed = false;
        for _ in 0..TURN_EVENTS {
            if Instant::now() >= self.until
                || (!self.mapped && self.opened.elapsed() >= MAP_TIMEOUT)
            {
                return Err(Error::Expired);
            }
            let mut kind = 0;
            // SAFETY: unique live handle on its creating thread; C writes only
            // the scalar event kind and retains no Rust pointer.
            match unsafe { fr_indicator_next(raw.as_ptr(), &raw mut kind) } {
                0 => break,
                1 => {}
                _ => return Err(Error::Unavailable),
            }
            match kind {
                0 => {}
                1 => draw = true,
                2 => { self.mapped = true; draw = true; }
                // Hidden/lost surfaces and local Deny/close dominate an Allow
                // elsewhere in this bounded turn. Never re-map a stopped UI.
                3..=5 => return Ok(State::Denied),
                6 if self.allow_ready() => allowed = true,
                // Mapping alone is not an interactive consent surface. A click
                // queued before the first completed draw cannot approve later.
                6 => {},
                _ => return Err(Error::Unavailable),
            }
        }
        if draw {
            // SAFETY: the same unique native handle, with constant C-owned UI
            // text; no peer content or callback crosses this boundary.
            if unsafe { fr_indicator_draw(raw.as_ptr()) } == 0 {
                return Err(Error::Unavailable);
            }
            self.drawn = self.mapped;
        }
        if Instant::now() >= self.until {
            return Err(Error::Expired);
        }
        if allowed {
            Ok(State::Allowed)
        } else if self.allow_ready() {
            Ok(State::Mapped)
        } else {
            Ok(State::Opening)
        }
    }
    fn allow_ready(&self) -> bool { self.mapped && self.drawn }
    pub fn close(&mut self) {
        if let Some(raw) = self.raw.take() {
            // SAFETY: this is the sole matching destructor on the creating
            // thread. C destroys its window before releasing input exclusion.
            unsafe { fr_indicator_close(raw.as_ptr()); }
        }
    }
}
impl Drop for Surface {
    fn drop(&mut self) { self.close(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapping_without_a_completed_draw_is_not_positive_consent_evidence() {
        let now = Instant::now();
        let mut surface = Surface {
            raw: None, opened: now, until: now + MAP_TIMEOUT,
            mapped: true, drawn: false, _thread: PhantomData,
        };
        assert!(!surface.allow_ready());
        surface.drawn = true;
        assert!(surface.allow_ready());
        surface.mapped = false;
        assert!(!surface.allow_ready());
    }
}
