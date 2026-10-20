//! Physical local file commands; the action key's whole gesture stays local.
//! Modifier presses/releases still use the ordinary remote input path. A key
//! pressed BEFORE the chord stays remote, including its repeat and release.
use super::Raw;

const NAMES: [[u8; 4]; 9] = [
    *b"LCTL", *b"RCTL", *b"LALT", *b"RALT", *b"LFSH", *b"RTSH", *b"FK08", *b"FK09", *b"FK10",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Command {
    Pause,
    Resume,
    Cancel,
}
impl Command {
    /// Only a request on this original selection. The existing sender still
    /// checks its connection, lease, deadlines and terminal state before work.
    pub(super) fn apply(self, control: &frd::native_files::SendControl) {
        let _accepted = match self {
            Self::Pause => control.pause(),
            Self::Resume => control.resume(),
            Self::Cancel => control.cancel(),
        };
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Route {
    Remote,
    Local(Option<Command>),
}
pub(super) struct Shortcuts {
    codes: [Option<u8>; 9],
    held: [bool; 9],
    local: [bool; 3],
}
impl Shortcuts {
    /// Missing or ambiguous physical keys disable this optional UI feature,
    /// never substitute symbols/keycodes or weaken the emergency escape.
    pub(super) fn new(names: &[[u8; 4]; 256]) -> Option<Self> {
        let mut codes = [None; 9];
        for (slot, name) in NAMES.iter().enumerate() {
            for (code, actual) in names.iter().enumerate() {
                if actual == name {
                    if code < 8 || codes[slot].is_some() {
                        return None;
                    }
                    codes[slot] = Some(u8::try_from(code).ok()?);
                }
            }
        }
        if codes[6..].iter().any(Option::is_none)
            || (0..3).any(|group| codes[2 * group..2 * group + 2].iter().all(Option::is_none))
        {
            return None;
        }
        Some(Self {
            codes,
            held: [false; 9],
            local: [false; 3],
        })
    }
    /// Call only after native lifecycle/synthetic checks AND timestamp checks.
    /// Disabled bindings still observe gestures: enabling during a held remote
    /// function key cannot steal its release or turn autorepeat into a command.
    pub(super) fn event(&mut self, event: Raw, enabled: bool) -> Route {
        if !matches!(event.kind, 1 | 2) {
            return Route::Remote;
        }
        let Some(slot) = self
            .codes
            .iter()
            .position(|code| code.is_some_and(|code| u32::from(code) == event.detail))
        else {
            return Route::Remote;
        };
        let down = event.kind == 1;
        let was_down = std::mem::replace(&mut self.held[slot], down);
        if slot < 6 {
            return Route::Remote;
        }
        let action = slot - 6;
        if down && !was_down {
            self.local[action] =
                enabled && (0..3).all(|group| self.held[2 * group] || self.held[2 * group + 1]);
            if self.local[action] {
                let command = [Command::Pause, Command::Resume, Command::Cancel][action];
                return Route::Local(Some(command));
            }
        }
        let local = self.local[action];
        if !down {
            self.local[action] = false;
        }
        if local {
            Route::Local(None)
        } else {
            Route::Remote
        }
    }
    pub(super) fn has_held(&self) -> bool {
        self.held.iter().any(|held| *held)
    }
    /// Release-only: a server snapshot never invents a press or a command.
    /// Keep gesture custody until its actual release. A late release after a
    /// stable clear remains local rather than becoming an unmatched remote up.
    pub(super) fn reconcile(&mut self, actual: &[u8; 32]) {
        for (code, held) in self.codes.iter().zip(&mut self.held) {
            *held &=
                code.is_some_and(|code| actual[usize::from(code / 8)] & (1 << (code % 8)) != 0);
        }
    }
}

#[cfg(test)]
mod tests;
