//! Native snapshots are release-only intersections with THIS producer's presses.
//! A query is discarded if any key/button transition crossed its server barrier:
//! two native replies are not an atomic snapshot and cannot erase a newer press.
use super::{ClientInstant, Decoder, Event, NATIVE_TURN_US, Native, Raw, StopReason, TURN, Target};
use fr_core::{
    held_state::HeldState,
    input::{PhysicalKey, PointerButton},
};
use std::{
    ffi::{c_int, c_void},
    thread,
};

// The existing wire sender admits at most one snapshot per 250ms of DISPATCH
// time and drops early samples. Native sampling must include the full allowed
// capture-to-dispatch age: otherwise an empty final snapshot could be dropped
// and sampling would stop with a remote key still held. With this spacing, even
// the latest valid prior dispatch precedes the next one by at least 250ms.
// No event is retained longer or retimestamped to achieve this guarantee.
pub(super) const INTERVAL_US: u64 = 250_000 + frd::session_startup::viewer_events::MAX_EVENT_AGE_US;
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(super) struct Snapshot {
    pub(super) keys: [u8; 32],
    pub(super) buttons: [u8; 32],
}
unsafe extern "C" {
    fn fr_viewer_input_held_begin(handle: *mut c_void) -> c_int;
    fn fr_viewer_input_held_poll(handle: *mut c_void, out: *mut Snapshot) -> c_int;
}
pub(super) struct Sampler {
    next: ClientInstant,
}
impl Sampler {
    pub(super) fn new(now: ClientInstant) -> Result<Self, StopReason> {
        Ok(Self { next: next(now)? })
    }
    pub(super) fn due(&self, decoder: &Decoder, now: ClientInstant) -> bool {
        decoder.has_held() && now >= self.next
    }
    pub(super) fn begin(&mut self, native: &Native, now: ClientInstant) -> Result<(), StopReason> {
        self.next = next(now)?;
        // SAFETY: live connection on its unique native owner thread. One pair
        // only; the following server barrier flushes these asynchronous requests.
        if unsafe { fr_viewer_input_held_begin(native.0.as_ptr()) } != 1 {
            return Err(StopReason::NativeFailure);
        }
        Ok(())
    }
}
fn next(now: ClientInstant) -> Result<ClientInstant, StopReason> {
    now.0
        .checked_add(INTERVAL_US)
        .map(ClientInstant)
        .ok_or(StopReason::Clock)
}
pub(super) fn complete(
    native: &Native,
    target: &impl Target,
    before: ClientInstant,
) -> Result<Snapshot, StopReason> {
    let mut snapshot = Snapshot::default();
    loop {
        if target
            .clock()?
            .0
            .checked_sub(before.0)
            .ok_or(StopReason::Clock)?
            >= NATIVE_TURN_US
        {
            return Err(StopReason::Expired);
        }
        // SAFETY: fixed writable output, same unique owner, no callback or
        // retained Rust pointer. Poll never waits for a server response.
        match unsafe { fr_viewer_input_held_poll(native.0.as_ptr(), &raw mut snapshot) } {
            0 => thread::sleep(TURN),
            1 => return Ok(snapshot),
            _ => return Err(StopReason::NativeFailure),
        }
    }
}
pub(super) fn stable(events: &[Raw]) -> bool {
    !events.iter().any(|e| (1..=4).contains(&e.kind))
}
fn bit(bits: &[u8; 32], index: usize) -> bool {
    bits[index / 8] & (1 << (index % 8)) != 0
}
impl Decoder {
    pub(super) fn has_held(&self) -> bool {
        self.keys.iter().any(|down| *down) || self.buttons.iter().any(|down| *down)
    }
    pub(super) fn physical_key(&self, index: usize) -> Result<PhysicalKey, StopReason> {
        let name = self.names.get(index).ok_or(StopReason::UnsupportedKey)?;
        let usage = super::super::key_names::KEY_NAMES
            .iter()
            .find(|(_, n)| n == name)
            .ok_or(StopReason::UnsupportedKey)?
            .0;
        PhysicalKey::new(usage).ok_or(StopReason::UnsupportedKey)
    }
    /// Unknown actual bits never become presses or adopted held gestures. Clear
    /// lost releases in the native decoder as well as the original remote owner.
    pub(super) fn reconcile(&mut self, actual: &Snapshot) -> Result<Event, StopReason> {
        let mut observed = HeldState::empty();
        for index in 0..self.keys.len() {
            self.keys[index] &= bit(&actual.keys, index);
            if self.keys[index] {
                observed.set_key(self.physical_key(index)?, true);
            }
        }
        for (index, button) in [
            (1, PointerButton::Primary),
            (2, PointerButton::Middle),
            (3, PointerButton::Secondary),
            (8, PointerButton::Back),
            (9, PointerButton::Forward),
        ] {
            self.buttons[index] &= bit(&actual.buttons, index);
            observed.set_button(button, self.buttons[index]);
        }
        Ok(Event::HeldState(observed))
    }
}
