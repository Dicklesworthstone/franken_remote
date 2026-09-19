//! A local emergency exit, independent of negotiated keyboard permissions.
//! Physical Ctrl+Alt+Shift+Escape; either side of each modifier is accepted.
//! No symbols, text, input grant, replay, or native state adoption lives here.
use super::{Raw, StopReason};

const NAMES: [[u8; 4]; 7] = [
    *b"LCTL", *b"RCTL", *b"LALT", *b"RALT", *b"LFSH", *b"RTSH", *b"ESC\0",
];

pub(super) struct LocalEscape {
    codes: [Option<u8>; 7],
    held: [bool; 7],
}
impl LocalEscape {
    pub(super) fn new(names: &[[u8; 4]; 256]) -> Result<Self, StopReason> {
        let mut codes = [None; 7];
        for (slot, name) in NAMES.iter().enumerate() {
            for (code, actual) in names.iter().enumerate() {
                if actual == name {
                    if code < 8 || codes[slot].is_some() {
                        return Err(StopReason::EscapeUnavailable);
                    }
                    codes[slot] =
                        Some(u8::try_from(code).map_err(|_| StopReason::EscapeUnavailable)?);
                }
            }
        }
        if codes[6].is_none()
            || (0..3).any(|group| codes[2 * group..2 * group + 2].iter().all(Option::is_none))
        {
            return Err(StopReason::EscapeUnavailable);
        }
        Ok(Self {
            codes,
            held: [false; 7],
        })
    }
    /// Called only after the native batch has passed lifecycle/synthetic checks.
    /// A chord may span batches, but no part of the triggering batch is exported.
    pub(super) fn inspect(&mut self, events: &[Raw]) -> Result<(), StopReason> {
        for event in events {
            if !matches!(event.kind, 1 | 2) {
                continue;
            }
            if let Some(slot) = self
                .codes
                .iter()
                .position(|code| code.is_some_and(|code| u32::from(code) == event.detail))
            {
                self.held[slot] = event.kind == 1;
                if self.held[6]
                    && (0..3).all(|group| self.held[2 * group] || self.held[2 * group + 1])
                {
                    return Err(StopReason::LocalEscape);
                }
            }
        }
        Ok(())
    }
    /// A stable native snapshot can clear a lost modifier release, never invent
    /// a press. This is the same release-only discipline as the remote held set.
    pub(super) fn reconcile(&mut self, actual: &[u8; 32]) {
        for (code, held) in self.codes.iter().zip(&mut self.held) {
            *held &=
                code.is_some_and(|code| actual[usize::from(code / 8)] & (1 << (code % 8)) != 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn names() -> [[u8; 4]; 256] {
        let mut names = [[0; 4]; 256];
        for (slot, name) in NAMES.iter().enumerate() {
            names[slot + 8] = *name;
        }
        names
    }
    fn key(slot: u32, pressed: bool) -> Raw {
        Raw {
            kind: if pressed { 1 } else { 2 },
            detail: slot + 8,
            ..Raw::default()
        }
    }
    #[test]
    fn either_side_and_every_modifier_press_order_escape() {
        for control in 0..=1 {
            for alt in 2..=3 {
                for shift in 4..=5 {
                    for order in [
                        [control, alt, shift],
                        [alt, control, shift],
                        [shift, alt, control],
                        [control, shift, alt],
                        [alt, shift, control],
                        [shift, control, alt],
                    ] {
                        let mut escape = LocalEscape::new(&names()).unwrap();
                        for slot in order {
                            escape.inspect(&[key(slot, true)]).unwrap();
                        }
                        assert_eq!(
                            escape.inspect(&[key(6, true)]),
                            Err(StopReason::LocalEscape)
                        );
                    }
                }
            }
        }
    }
    #[test]
    fn incomplete_chords_release_and_repeat_do_not_escape() {
        for omitted in [0, 2, 4] {
            let mut escape = LocalEscape::new(&names()).unwrap();
            for slot in [0, 2, 4, 6] {
                if slot != omitted {
                    escape.inspect(&[key(slot, true), key(slot, true)]).unwrap();
                }
            }
        }
        let mut escape = LocalEscape::new(&names()).unwrap();
        escape
            .inspect(&[
                key(0, true),
                key(2, true),
                key(4, true),
                key(0, false),
                key(6, true),
            ])
            .unwrap();
    }
    #[test]
    fn stable_snapshot_clears_only_locally_seen_modifier_presses() {
        let mut escape = LocalEscape::new(&names()).unwrap();
        escape
            .inspect(&[key(0, true), key(2, true), key(4, true)])
            .unwrap();
        escape.reconcile(&[0; 32]);
        escape.inspect(&[key(6, true)]).unwrap();
        escape.reconcile(&[255; 32]);
        // The snapshot must not adopt held modifiers and complete a chord.
        escape.inspect(&[key(6, true)]).unwrap();
    }
    #[test]
    fn unavailable_or_ambiguous_physical_escape_is_refused() {
        let mut names = names();
        names[14] = [0; 4];
        assert!(matches!(
            LocalEscape::new(&names),
            Err(StopReason::EscapeUnavailable)
        ));
        let mut duplicate = super::tests::names();
        duplicate[80] = NAMES[0];
        assert!(matches!(
            LocalEscape::new(&duplicate),
            Err(StopReason::EscapeUnavailable)
        ));
    }
}
