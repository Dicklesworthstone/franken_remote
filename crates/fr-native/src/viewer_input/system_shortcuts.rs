//! One explicit, asynchronous X11 keyboard-grab attempt on the original window.
//! This local request changes neither input timestamps nor session authority.
//! The owning native connection releases its grab on every exit path.
use super::{Capability, NATIVE_TURN_US, Native, StopReason, TURN, Target, c_int, c_void};

unsafe extern "C" {
    fn fr_viewer_input_grab_begin(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_grab_poll(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_release_begin(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_release_poll(handle: *mut c_void) -> c_int;
}

pub(super) fn acquire(native: &Native, target: &impl Target) -> Result<(), StopReason> {
    if !target.capabilities().contains(Capability::Keys) {
        return Err(StopReason::SystemShortcutsUnavailable);
    }
    let before = target.clock()?;
    // SAFETY: the unique live native owner remains on this thread. The request
    // can address only its original window; C retains no Rust pointer.
    if unsafe { fr_viewer_input_grab_begin(native.0.as_ptr()) } != 1 {
        return Err(StopReason::NativeFailure);
    }
    loop {
        within_turn(before.0, target.clock()?.0)?;
        // SAFETY: same live owner, exactly one bounded asynchronous attempt.
        let status = unsafe { fr_viewer_input_grab_poll(native.0.as_ptr()) };
        // A late reply never renews the original clock/authority or becomes a
        // Running state. NativeRun fences before disconnecting the grab owner.
        within_turn(before.0, target.clock()?.0)?;
        match status {
            0 => std::thread::sleep(TURN),
            1 => return Ok(()),
            -2 => return Err(StopReason::SystemShortcutsUnavailable),
            -3 => return Err(StopReason::FocusLost),
            -4 => return Err(StopReason::InitialHeld),
            -5 => return Err(StopReason::WindowChanged),
            _ => return Err(StopReason::NativeFailure),
        }
    }
}

fn within_turn(before: u64, now: u64) -> Result<(), StopReason> {
    if now.checked_sub(before).ok_or(StopReason::Clock)? >= NATIVE_TURN_US {
        return Err(StopReason::Expired);
    }
    Ok(())
}

/// Negative cleanup after the original input fence, never a new grant or input
/// timestamp. A stalled X server cannot keep this owner waiting for a title ack;
/// its connection is disconnected after this independent fixed cleanup budget.
pub(super) fn release(native: &Native) {
    let until = std::time::Instant::now() + std::time::Duration::from_micros(NATIVE_TURN_US);
    // SAFETY: the original live native owner remains on its sole owning thread.
    if unsafe { fr_viewer_input_release_begin(native.0.as_ptr()) } != 1 {
        return;
    }
    while std::time::Instant::now() < until {
        // SAFETY: the same owner, one outstanding fixed-size cleanup reply.
        if unsafe { fr_viewer_input_release_poll(native.0.as_ptr()) } != 0 {
            return;
        }
        std::thread::sleep(TURN);
    }
}
