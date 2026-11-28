//! Explicit local file-dialog interval on the original native producer.
//! Input is not captured during this interval. It is never a focus exception
//! for a running producer, a new source, or a way to revive a stopped grant.
mod state;
use super::{
    Decoder, NATIVE_TURN_US, Native, StopReason, TURN, Target, Window, batch, display_switch,
    escape, file_keys, held, initialize_batch, system_shortcuts, timing::Timeline,
};
use crate::viewer_window::{Status, WindowControl};
use frd::session_startup::viewer_events::Layout;
pub(crate) use state::Gate;
pub(super) use state::Phase;
use std::{
    ffi::{c_int, c_void},
    sync::Arc,
    thread,
};

unsafe extern "C" {
    fn fr_viewer_input_choice_now() -> u64;
    fn fr_viewer_input_choice_begin(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_choice_probe_begin(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_choice_probe_poll(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_choice_end(handle: *mut c_void) -> c_int;
}

pub(super) fn now() -> Result<u64, ()> {
    // SAFETY: a bounded CLOCK_BOOTTIME scalar read; no handle or pointer.
    let value = unsafe { fr_viewer_input_choice_now() };
    (value != 0).then_some(value).ok_or(())
}

/// Retained by the selection worker through exact portal cleanup AND the
/// original input producer's positively checked return. Unknown cleanup stops
/// the original session; dropping an unfinished dialog cannot enable a new one.
pub(crate) struct Pause {
    window: WindowControl,
    gate: Arc<Gate>,
    serial: u64,
}
impl Pause {
    pub(crate) fn new(window: WindowControl, gate: Arc<Gate>) -> Result<Self, ()> {
        let serial = gate.request(now()?)?;
        Ok(Self {
            window,
            gate,
            serial,
        })
    }

    fn phase(&self) -> Result<Phase, ()> {
        if self.window.status() != Status::Mapped {
            self.gate.stop();
            return Err(());
        }
        self.gate.phase(self.serial, now()?)
    }

    pub(crate) fn suspended(&self) -> Result<bool, ()> {
        match self.phase()? {
            Phase::Requested | Phase::Pausing => Ok(false),
            Phase::Paused => Ok(true),
            _ => Err(()),
        }
    }

    /// Only the original selection worker calls this, after positive native
    /// portal closure. Neither a stop request nor a timer is that evidence.
    pub(crate) fn resume(&self) -> Result<(), ()> {
        self.phase()?;
        self.gate.resume(self.serial, now()?)
    }

    pub(crate) fn resumed(&self) -> Result<bool, ()> {
        match self.phase()? {
            Phase::Returning => Ok(false),
            Phase::Resumed => Ok(true),
            _ => Err(()),
        }
    }
}
impl Drop for Pause {
    fn drop(&mut self) {
        if !self.gate.finish(self.serial) {
            self.window.stop();
        }
    }
}

pub(super) fn requested(gate: &Gate) -> Result<Option<u64>, StopReason> {
    gate.requested(now().map_err(|()| StopReason::Clock)?)
        .map_err(|()| StopReason::Closed)
}

fn phase(gate: &Gate, serial: u64) -> Result<Phase, StopReason> {
    gate.phase(serial, now().map_err(|()| StopReason::Clock)?)
        .map_err(|()| StopReason::Closed)
}

/// Query and drain one original native turn. A response cannot overwrite a
/// later focus/input/visibility change; C retains that negative latch. All
/// paused gestures are consumed locally, including the final return barrier.
fn probe(
    native: &Native,
    target: &mut impl Target,
    window: Window,
    escape: &mut escape::LocalEscape,
    gate: &Gate,
    serial: u64,
) -> Result<bool, StopReason> {
    let started = target.clock()?;
    // SAFETY: this thread's one native owner; only fixed asynchronous queries.
    if unsafe { fr_viewer_input_choice_probe_begin(native.0.as_ptr()) } != 1 {
        return Err(StopReason::NativeFailure);
    }
    let mut answered = false;
    loop {
        phase(gate, serial)?;
        let before = target.clock()?;
        if before.0.checked_sub(started.0).ok_or(StopReason::Clock)? >= NATIVE_TURN_US {
            return Err(StopReason::Expired);
        }
        let (events, used, _) = batch(native, target, window, None, before, true)?;
        escape.inspect(&events[..used])?;
        // SAFETY: same original probe; all replies stay within this C owner.
        let status = unsafe { fr_viewer_input_choice_probe_poll(native.0.as_ptr()) };
        let after = target.clock()?;
        if after.0.checked_sub(started.0).ok_or(StopReason::Clock)? >= NATIVE_TURN_US {
            return Err(StopReason::Expired);
        }
        match status {
            0 => thread::sleep(TURN),
            1 | 2 if answered => return Ok(status == 1),
            1 | 2 => {
                // A reply read can buffer native events. Drain another full
                // server barrier before accepting the original probe result.
                answered = true;
            }
            _ => return Err(StopReason::NativeFailure),
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one original producer and its bounded native ledgers"
)]
pub(super) fn run(
    native: &Native,
    target: &mut impl Target,
    window: Window,
    layout: &Layout,
    names: &[[u8; 4]; 256],
    decoder: &mut Decoder,
    shortcuts: &mut Option<file_keys::Shortcuts>,
    display: &mut Option<display_switch::Shortcut>,
    escape: &mut escape::LocalEscape,
    system_shortcuts: bool,
    gate: &Gate,
    serial: u64,
    clean_batch: bool,
) -> Result<Option<(Timeline, held::Sampler)>, StopReason> {
    // Starting while a physical gesture is held would need explicit ordered
    // releases. Refuse before changing native state instead; normal capture
    // keeps the original releases and all already admitted effects/receipts.
    if !clean_batch
        || decoder.has_held()
        || super::local_keys_held(target, shortcuts.as_ref(), display.as_ref())
        || super::text::has_held(native)
        || !target.input_idle()
    {
        gate.refuse(serial, now().map_err(|()| StopReason::Clock)?)
            .map_err(|()| StopReason::Closed)?;
        return Ok(None);
    }
    // SAFETY: the original native owner, before any portal is opened. A zero
    // result made no mutation; failures after native work are terminal.
    match unsafe { fr_viewer_input_choice_begin(native.0.as_ptr()) } {
        0 => {
            gate.refuse(serial, now().map_err(|()| StopReason::Clock)?)
                .map_err(|()| StopReason::Closed)?;
            return Ok(None);
        }
        1 => {}
        _ => return Err(StopReason::NativeFailure),
    }
    gate.begin(serial, now().map_err(|()| StopReason::Clock)?)
        .map_err(|()| StopReason::Closed)?;
    while !probe(native, target, window, escape, gate, serial)? {
        thread::sleep(TURN);
    }
    gate.paused(serial, now().map_err(|()| StopReason::Clock)?)
        .map_err(|()| StopReason::Closed)?;
    // No Source::push, text Send, local file shortcut or remote input occurs
    // here. Revocation, native lifecycle and local Escape are still serviced.
    while phase(gate, serial)? == Phase::Paused {
        let before = target.clock()?;
        let (events, used, _) = batch(native, target, window, None, before, true)?;
        escape.inspect(&events[..used])?;
        thread::sleep(std::time::Duration::from_millis(10));
    }
    if phase(gate, serial)? != Phase::Returning {
        return Err(StopReason::Closed);
    }
    let mut grabbed = !system_shortcuts;
    loop {
        phase(gate, serial)?;
        if !probe(native, target, window, escape, gate, serial)? {
            thread::sleep(std::time::Duration::from_millis(10));
            continue;
        }
        if !grabbed {
            // The original explicit shortcut choice survives this local pause.
            // Its same connection alone reacquires its own keyboard grab.
            system_shortcuts::acquire(native, target)?;
            grabbed = true;
            continue;
        }
        // SAFETY: only the positively probed original owner can leave pause.
        match unsafe { fr_viewer_input_choice_end(native.0.as_ptr()) } {
            0 => continue,
            1 => break,
            _ => return Err(StopReason::NativeFailure),
        }
    }
    // Old portal/menu/input events can never acquire this new local timeline.
    // Reuse the original Source and Layout, with fresh empty native ledgers.
    *decoder = Decoder::new(names, target.capabilities());
    *shortcuts = file_keys::Shortcuts::new(names);
    *display = display_switch::Shortcut::new(names);
    *escape = escape::LocalEscape::new(names)?;
    let before = target.clock()?;
    let (events, used, barrier) = batch(native, target, window, None, before, true)?;
    escape.inspect(&events[..used])?;
    initialize_batch(
        &events[..used],
        layout,
        target,
        decoder,
        shortcuts.as_mut(),
        display.as_mut(),
    )?;
    let sampler = held::Sampler::new(target.clock()?)?;
    gate.resumed(serial, now().map_err(|()| StopReason::Clock)?)
        .map_err(|()| StopReason::Closed)?;
    Ok(Some((Timeline::new(barrier, before), sampler)))
}
