//! Bounded keyboard/button state for release-only reconciliation. A snapshot is
//! not an input command and never authorizes a press. Diagnostics hide its bits.
use crate::{
    ids::{InputLeaseId, RemoteSessionId},
    input::{PhysicalKey, PointerButton},
};
use core::fmt;

pub const KEY_BITMAP_BYTES: usize = 32;
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct HeldState {
    keys: [u8; KEY_BITMAP_BYTES],
    buttons: u8,
}
impl HeldState {
    pub const fn empty() -> Self {
        Self {
            keys: [0; KEY_BITMAP_BYTES],
            buttons: 0,
        }
    }
    /// Reject reserved keyboard usages and button bits rather than silently
    /// truncating an unsupported set. The page is always USB keyboard 0x07.
    pub fn from_bits(keys: [u8; KEY_BITMAP_BYTES], buttons: u8) -> Option<Self> {
        if buttons & !0x1f != 0 {
            return None;
        }
        for usage in 0_u16..256 {
            if keys[usize::from(usage / 8)] & (1 << (usage % 8)) != 0
                && PhysicalKey::new(usage).is_none()
            {
                return None;
            }
        }
        Some(Self { keys, buttons })
    }
    pub const fn key_bits(&self) -> &[u8; KEY_BITMAP_BYTES] {
        &self.keys
    }
    pub const fn button_bits(&self) -> u8 {
        self.buttons
    }
    pub fn key(&self, key: PhysicalKey) -> bool {
        self.keys[usize::from(key.usage() / 8)] & (1 << (key.usage() % 8)) != 0
    }
    pub fn set_key(&mut self, key: PhysicalKey, held: bool) {
        let byte = &mut self.keys[usize::from(key.usage() / 8)];
        let mask = 1 << (key.usage() % 8);
        if held {
            *byte |= mask;
        } else {
            *byte &= !mask;
        }
    }
    pub const fn button(&self, button: PointerButton) -> bool {
        self.buttons & (1 << (button as u8 - 1)) != 0
    }
    pub fn set_button(&mut self, button: PointerButton, held: bool) {
        let mask = 1 << (button as u8 - 1);
        if held {
            self.buttons |= mask;
        } else {
            self.buttons &= !mask;
        }
    }
}
impl fmt::Debug for HeldState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HeldState([redacted])")
    }
}
/// Sent on the SAME reliable stream as actions. `next_action` is the sender's
/// next reliable sequence after all preceding actions. It prevents a snapshot
/// produced before a newer press from releasing that press. Snapshot sequence
/// gaps are allowed (replaceable state); action gaps are never allowed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct HeldStateRequest {
    pub session: RemoteSessionId,
    pub lease: InputLeaseId,
    pub sequence: u64,
    pub next_action: u64,
    pub held: HeldState,
}
impl fmt::Debug for HeldStateRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("HeldStateRequest([redacted])")
    }
}
