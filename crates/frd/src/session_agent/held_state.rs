//! Remote-only key/button held-state tracking and crash uncertainty reporting (plan §§5.3, 7.3, 15.2).
//!
//! Tracks only remote injections, not physical local user input.
//! Synthesizes cleanup release operations on disconnect or revoke.
//! Reports uncertain release states honestly after an input-process crash,
//! refusing to invent certainty or assume keys were released.

use fr_core::{
    held_state::{HeldState, KEY_BITMAP_BYTES},
    input::{KeyTransition, PhysicalKey, PointerButton},
    input_submission::Operation,
};

/// Degree of certainty regarding physical/compositor key release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseCertainty {
    /// Clean state: no remote keys or buttons are held.
    Clean,
    /// Confirmed released: all synthetic release operations were confirmed submitted by native sink.
    ConfirmedReleased,
    /// Uncertain: the input-handling worker process crashed or panicked while keys were held.
    /// It is unknown whether the OS still considers the keys held down.
    UncertainDueToCrash,
    /// Uncertain: a native OS submission call returned Unknown or failed midway through a batch.
    UncertainDueToNativeError,
}

/// Honest report of release state after an interruption or crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UncertainReleaseReport {
    /// Number of keys that were held at the time of the crash or failure.
    pub keys_uncertain: u16,
    /// Number of mouse buttons that were held at the time of the crash or failure.
    pub buttons_uncertain: u8,
    /// Assessment of release certainty.
    pub certainty: ReleaseCertainty,
}

/// Tracks remotely injected held keys and buttons for synthetic cleanup and audit.
#[derive(Clone)]
pub struct RemoteHeldTracker {
    keys: [bool; 256],
    buttons: [bool; 5],
    last_certainty: Option<ReleaseCertainty>,
}

impl Default for RemoteHeldTracker {
    fn default() -> Self {
        Self {
            keys: [false; 256],
            buttons: [false; 5],
            last_certainty: None,
        }
    }
}

impl RemoteHeldTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an injected operation that was confirmed admitted/submitted.
    /// Only remotely injected operations are tracked; local input must never touch this.
    pub fn record_injected_operation(&mut self, op: &Operation) {
        match op {
            Operation::Key { key, transition } => {
                let usage = key.usage() as usize;
                if usage < 256 {
                    match transition {
                        KeyTransition::Press => self.keys[usage] = true,
                        KeyTransition::Release => self.keys[usage] = false,
                        KeyTransition::Repeat => {}
                    }
                }
            }
            Operation::Button { button, pressed } => {
                let idx = *button as usize - 1;
                if idx < 5 {
                    self.buttons[idx] = *pressed;
                }
            }
            _ => {}
        }
    }

    /// Check if a specific physical key is currently held by remote injection.
    pub fn is_key_held(&self, key: PhysicalKey) -> bool {
        let usage = key.usage() as usize;
        if usage < 256 { self.keys[usage] } else { false }
    }

    /// Check if a specific pointer button is currently held by remote injection.
    pub fn is_button_held(&self, button: PointerButton) -> bool {
        let idx = button as usize - 1;
        if idx < 5 { self.buttons[idx] } else { false }
    }

    /// Number of currently held remote keys.
    pub fn held_key_count(&self) -> usize {
        self.keys.iter().filter(|&&held| held).count()
    }

    /// Number of currently held remote pointer buttons.
    pub fn held_button_count(&self) -> usize {
        self.buttons.iter().filter(|&&held| held).count()
    }

    /// True if no remote keys or buttons are currently held.
    pub fn is_clean(&self) -> bool {
        self.held_key_count() == 0 && self.held_button_count() == 0
    }

    /// Convert internal state into a core `HeldState` bitmap for wire/audit.
    pub fn snapshot(&self) -> HeldState {
        let mut keys_bitmap = [0u8; KEY_BITMAP_BYTES];
        let mut buttons_bitmap = 0u8;

        for (usage, &held) in self.keys.iter().enumerate() {
            if held {
                keys_bitmap[usage / 8] |= 1 << (usage % 8);
            }
        }

        for (btn_idx, &held) in self.buttons.iter().enumerate() {
            if held {
                buttons_bitmap |= 1 << btn_idx;
            }
        }

        HeldState::from_bits(keys_bitmap, buttons_bitmap).unwrap_or_else(HeldState::empty)
    }

    /// Synthesize all required release operations to clear currently held keys and buttons.
    ///
    /// This generates:
    /// 1. `Operation::Button { pressed: false }` for each held pointer button.
    /// 2. `Operation::Key { transition: KeyTransition::Release }` for each held key.
    ///
    /// Clears internal state and sets certainty to `ConfirmedReleased`.
    pub fn synthesize_cleanup_releases(&mut self) -> Vec<Operation> {
        let mut releases = Vec::new();

        // 1. Release buttons first
        for (btn_idx, held) in self.buttons.iter_mut().enumerate() {
            if *held {
                let button = match btn_idx {
                    0 => PointerButton::Primary,
                    1 => PointerButton::Secondary,
                    2 => PointerButton::Middle,
                    3 => PointerButton::Back,
                    _ => PointerButton::Forward,
                };
                releases.push(Operation::Button {
                    button,
                    pressed: false,
                });
                *held = false;
            }
        }

        // 2. Release keys
        for (usage, held) in self.keys.iter_mut().enumerate() {
            if *held {
                if let Some(key) = u16::try_from(usage).ok().and_then(PhysicalKey::new) {
                    releases.push(Operation::Key {
                        key,
                        transition: KeyTransition::Release,
                    });
                }
                *held = false;
            }
        }

        self.last_certainty = Some(ReleaseCertainty::ConfirmedReleased);
        releases
    }

    /// Record a worker process crash or hang while keys were held.
    ///
    /// This produces an honest `UncertainReleaseReport` documenting how many keys/buttons
    /// were held without pretending they were cleanly released.
    pub fn record_worker_crash(&mut self) -> UncertainReleaseReport {
        let keys_uncertain = u16::try_from(self.held_key_count()).unwrap_or(u16::MAX);
        let buttons_uncertain = u8::try_from(self.held_button_count()).unwrap_or(u8::MAX);

        let certainty = if keys_uncertain == 0 && buttons_uncertain == 0 {
            ReleaseCertainty::Clean
        } else {
            ReleaseCertainty::UncertainDueToCrash
        };

        self.last_certainty = Some(certainty);

        UncertainReleaseReport {
            keys_uncertain,
            buttons_uncertain,
            certainty,
        }
    }

    /// Record a native submission failure or unknown effect.
    pub fn record_native_error(&mut self) -> UncertainReleaseReport {
        let keys_uncertain = u16::try_from(self.held_key_count()).unwrap_or(u16::MAX);
        let buttons_uncertain = u8::try_from(self.held_button_count()).unwrap_or(u8::MAX);

        let certainty = if keys_uncertain == 0 && buttons_uncertain == 0 {
            ReleaseCertainty::Clean
        } else {
            ReleaseCertainty::UncertainDueToNativeError
        };

        self.last_certainty = Some(certainty);

        UncertainReleaseReport {
            keys_uncertain,
            buttons_uncertain,
            certainty,
        }
    }

    /// Query the last known release certainty.
    pub fn last_certainty(&self) -> ReleaseCertainty {
        self.last_certainty.unwrap_or(if self.is_clean() {
            ReleaseCertainty::Clean
        } else {
            ReleaseCertainty::UncertainDueToCrash
        })
    }
}
