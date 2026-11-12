//! Windows keyboard scan codes (set 1, as `WM_KEYDOWN`'s lParam carries
//! them: bits 16-23 plus the extended-key flag, bit 24) to USB HID keyboard
//! usages (page 0x07), the physical keys the session sends. Physical, not
//! characters: the host types with its own layout. Platform-neutral so every
//! CI host tests it.

/// The HID usage of scan code `scan` (`extended`: the E0-prefixed key), or
/// `None` for a key the table does not carry.
#[allow(clippy::too_many_lines, reason = "one lookup table")]
pub fn usage(scan: u8, extended: bool) -> Option<u16> {
    if extended {
        return Some(match scan {
            0x1C => 0x58, // keypad Enter
            0x1D => 0xE4, // right Ctrl
            0x35 => 0x54, // keypad /
            0x37 => 0x46, // Print Screen
            0x38 => 0xE6, // right Alt (AltGr)
            // Windows reports Num Lock with the extended flag and Pause
            // without it (the reverse of the raw set-1 bytes).
            0x45 => 0x53, // Num Lock
            0x46 => 0x48, // Ctrl+Break: Pause
            0x47 => 0x4A, // Home
            0x48 => 0x52, // Up
            0x49 => 0x4B, // Page Up
            0x4B => 0x50, // Left
            0x4D => 0x4F, // Right
            0x4F => 0x4D, // End
            0x50 => 0x51, // Down
            0x51 => 0x4E, // Page Down
            0x52 => 0x49, // Insert
            0x53 => 0x4C, // Delete
            0x5B => 0xE3, // left Windows
            0x5C => 0xE7, // right Windows
            0x5D => 0x65, // Application (menu)
            _ => return None,
        });
    }
    Some(match scan {
        0x01 => 0x29, // Escape
        // 1..9 then 0.
        0x02..=0x0A => 0x1E + u16::from(scan - 0x02),
        0x0B => 0x27,
        0x0C => 0x2D, // -
        0x0D => 0x2E, // =
        0x0E => 0x2A, // Backspace
        0x0F => 0x2B, // Tab
        0x10 => 0x14, // Q
        0x11 => 0x1A, // W
        0x12 => 0x08, // E
        0x13 => 0x15, // R
        0x14 => 0x17, // T
        0x15 => 0x1C, // Y
        0x16 => 0x18, // U
        0x17 => 0x0C, // I
        0x18 => 0x12, // O
        0x19 => 0x13, // P
        0x1A => 0x2F, // [
        0x1B => 0x30, // ]
        0x1C => 0x28, // Enter
        0x1D => 0xE0, // left Ctrl
        0x1E => 0x04, // A
        0x1F => 0x16, // S
        0x20 => 0x07, // D
        0x21 => 0x09, // F
        0x22 => 0x0A, // G
        0x23 => 0x0B, // H
        0x24 => 0x0D, // J
        0x25 => 0x0E, // K
        0x26 => 0x0F, // L
        0x27 => 0x33, // ;
        0x28 => 0x34, // '
        0x29 => 0x35, // `
        0x2A => 0xE1, // left Shift
        0x2B => 0x31, // backslash
        0x2C => 0x1D, // Z
        0x2D => 0x1B, // X
        0x2E => 0x06, // C
        0x2F => 0x19, // V
        0x30 => 0x05, // B
        0x31 => 0x11, // N
        0x32 => 0x10, // M
        0x33 => 0x36, // ,
        0x34 => 0x37, // .
        0x35 => 0x38, // /
        0x36 => 0xE5, // right Shift
        0x37 => 0x55, // keypad *
        0x38 => 0xE2, // left Alt
        0x39 => 0x2C, // Space
        0x3A => 0x39, // Caps Lock
        // F1..F10.
        0x3B..=0x44 => 0x3A + u16::from(scan - 0x3B),
        0x45 => 0x48, // Pause
        0x46 => 0x47, // Scroll Lock
        0x47 => 0x5F, // keypad 7
        0x48 => 0x60, // keypad 8
        0x49 => 0x61, // keypad 9
        0x4A => 0x56, // keypad -
        0x4B => 0x5C, // keypad 4
        0x4C => 0x5D, // keypad 5
        0x4D => 0x5E, // keypad 6
        0x4E => 0x57, // keypad +
        0x4F => 0x59, // keypad 1
        0x50 => 0x5A, // keypad 2
        0x51 => 0x5B, // keypad 3
        0x52 => 0x62, // keypad 0
        0x53 => 0x63, // keypad .
        0x56 => 0x64, // the ISO key between left Shift and Z
        0x57 => 0x44, // F11
        0x58 => 0x45, // F12
        0x70 => 0x88, // Katakana/Hiragana
        0x73 => 0x87, // Ro
        0x79 => 0x8A, // Henkan
        0x7B => 0x8B, // Muhenkan
        0x7D => 0x89, // Yen
        _ => return None,
    })
}

/// One keyboard message as the window procedure sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMessage {
    pub scan: u8,
    pub extended: bool,
    pub pressed: bool,
    /// The message time (milliseconds, `GetMessageTime`).
    pub time: u32,
}

/// Whether `this` is the left Ctrl Windows synthesizes around `AltGr` on
/// layouts that have it: a left Ctrl transition (scan 0x1D, not extended)
/// queued directly ahead of a right Alt transition (scan 0x38, extended) in
/// the same direction with the same message time. The host types with its
/// own layout, where right Alt is `AltGr`; forwarding the synthetic Ctrl
/// would make every `AltGr` character Ctrl+`AltGr` (`@` on a German layout
/// arrives as Ctrl+@). A real left Ctrl is a separate keystroke with its own
/// time.
pub fn synthetic_ctrl(this: KeyMessage, next: Option<KeyMessage>) -> bool {
    this.scan == 0x1D
        && !this.extended
        && next.is_some_and(|next| {
            next.scan == 0x38
                && next.extended
                && next.pressed == this.pressed
                && next.time == this.time
        })
}

/// Key transitions the session has not taken yet, in typing order. The
/// session answers a new press `Busy` while frd's input queue is backed up;
/// that press, and every transition typed after it, waits here and goes on a
/// later turn, so the host sees exactly the local order (Shift still held
/// over a deferred letter, a release never ahead of its press).
#[derive(Debug, Default)]
pub struct Deferred {
    queue: std::collections::VecDeque<(u16, bool)>,
}

/// Presses past this many waiting transitions are dropped (a stalled host
/// never grows the queue without bound). Releases always queue: they only
/// follow presses, and a release of a key never pressed sends nothing.
pub const MAX_DEFERRED: usize = 64;

impl Deferred {
    /// Queues `(usage, pressed)` behind any waiting transitions, then sends
    /// in order. `send` returns false when the session answered `Busy`; it
    /// stops the drain and keeps that transition first.
    pub fn push(&mut self, usage: u16, pressed: bool, send: impl FnMut(u16, bool) -> bool) {
        if pressed && self.queue.len() >= MAX_DEFERRED {
            return;
        }
        self.queue.push_back((usage, pressed));
        self.drain(send);
    }
    /// Sends waiting transitions in order until the session is busy again.
    pub fn drain(&mut self, mut send: impl FnMut(u16, bool) -> bool) {
        while let Some(&(usage, pressed)) = self.queue.front() {
            if !send(usage, pressed) {
                return;
            }
            self.queue.pop_front();
        }
    }
    /// Forgets every waiting transition (focus lost: an unsent press must
    /// never reach the host afterwards).
    pub fn clear(&mut self) {
        self.queue.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a session took. New presses are refused (`Busy`) while `busy`
    /// is set; releases always go, as in the session core.
    struct Host {
        busy: bool,
        sent: Vec<(u16, bool)>,
    }
    impl Host {
        fn send(&mut self, usage: u16, pressed: bool) -> bool {
            if pressed && self.busy {
                return false;
            }
            self.sent.push((usage, pressed));
            true
        }
    }

    #[test]
    fn only_the_ctrl_windows_synthesizes_for_altgr_is_dropped() {
        let message = |scan, extended, pressed, time| KeyMessage {
            scan,
            extended,
            pressed,
            time,
        };
        let ctrl = message(0x1D, false, true, 1000);
        let altgr = message(0x38, true, true, 1000);
        assert!(synthetic_ctrl(ctrl, Some(altgr)));
        // The release pair, too.
        assert!(synthetic_ctrl(
            message(0x1D, false, false, 2000),
            Some(message(0x38, true, false, 2000))
        ));
        // Planted negatives: a real Ctrl (its own time), right Ctrl, left
        // Alt, opposite directions, nothing queued, and AltGr itself.
        assert!(!synthetic_ctrl(ctrl, Some(message(0x38, true, true, 1001))));
        assert!(!synthetic_ctrl(
            message(0x1D, true, true, 1000),
            Some(altgr)
        ));
        assert!(!synthetic_ctrl(
            ctrl,
            Some(message(0x38, false, true, 1000))
        ));
        assert!(!synthetic_ctrl(
            ctrl,
            Some(message(0x38, true, false, 1000))
        ));
        assert!(!synthetic_ctrl(ctrl, None));
        assert!(!synthetic_ctrl(altgr, Some(altgr)));
    }

    #[test]
    fn a_busy_press_holds_everything_typed_after_it_in_order() {
        const SHIFT: u16 = 0xE1;
        const A: u16 = 0x04;
        let mut deferred = Deferred::default();
        let mut host = Host {
            busy: false,
            sent: Vec::new(),
        };
        deferred.push(SHIFT, true, |u, p| host.send(u, p));
        host.busy = true;
        // A is deferred; A's release and Shift's release wait behind it
        // (sent now, Shift would be up before A went down).
        deferred.push(A, true, |u, p| host.send(u, p));
        deferred.push(A, false, |u, p| host.send(u, p));
        deferred.push(SHIFT, false, |u, p| host.send(u, p));
        assert_eq!(host.sent, vec![(SHIFT, true)]);
        // A later turn while still busy moves nothing.
        deferred.drain(|u, p| host.send(u, p));
        assert_eq!(host.sent, vec![(SHIFT, true)]);
        assert!(!deferred.is_empty());
        // Ready again: the rest, in typing order.
        host.busy = false;
        deferred.drain(|u, p| host.send(u, p));
        assert_eq!(
            host.sent,
            vec![(SHIFT, true), (A, true), (A, false), (SHIFT, false)]
        );
        assert!(deferred.is_empty());
    }

    #[test]
    fn a_stalled_session_drops_new_presses_but_never_releases() {
        let mut deferred = Deferred::default();
        let mut host = Host {
            busy: true,
            sent: Vec::new(),
        };
        for usage in 0..u16::try_from(MAX_DEFERRED + 10).unwrap() {
            deferred.push(usage, true, |u, p| host.send(u, p));
        }
        assert_eq!(deferred.queue.len(), MAX_DEFERRED);
        deferred.push(7, false, |u, p| host.send(u, p));
        assert_eq!(deferred.queue.len(), MAX_DEFERRED + 1);
        assert_eq!(deferred.queue.back(), Some(&(7, false)));
        assert_eq!(host.sent, Vec::new());
        // Focus lost: nothing that waited is ever sent, even once ready.
        deferred.clear();
        host.busy = false;
        deferred.drain(|u, p| host.send(u, p));
        assert_eq!(host.sent, Vec::new());
    }

    #[test]
    fn letters_digits_and_function_keys_follow_the_hid_layout() {
        // QWERTY rows: Q W E ... P, A ... L, Z ... M.
        let row = |scans: core::ops::RangeInclusive<u8>| {
            scans.map(|s| usage(s, false).unwrap()).collect::<Vec<_>>()
        };
        assert_eq!(
            row(0x10..=0x19),
            [0x14, 0x1A, 0x08, 0x15, 0x17, 0x1C, 0x18, 0x0C, 0x12, 0x13]
        );
        assert_eq!(
            row(0x1E..=0x26),
            [0x04, 0x16, 0x07, 0x09, 0x0A, 0x0B, 0x0D, 0x0E, 0x0F]
        );
        assert_eq!(row(0x2C..=0x32), [0x1D, 0x1B, 0x06, 0x19, 0x05, 0x11, 0x10]);
        assert_eq!(
            row(0x02..=0x0B),
            [0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27]
        );
        assert_eq!(
            row(0x3B..=0x44),
            [0x3A, 0x3B, 0x3C, 0x3D, 0x3E, 0x3F, 0x40, 0x41, 0x42, 0x43]
        );
        assert_eq!(
            (usage(0x57, false), usage(0x58, false)),
            (Some(0x44), Some(0x45))
        );
    }

    #[test]
    fn the_extended_flag_separates_left_from_right_and_keypad_from_navigation() {
        assert_eq!(usage(0x1D, false), Some(0xE0));
        assert_eq!(usage(0x1D, true), Some(0xE4));
        assert_eq!(usage(0x38, false), Some(0xE2));
        assert_eq!(usage(0x38, true), Some(0xE6));
        assert_eq!(usage(0x1C, false), Some(0x28));
        assert_eq!(usage(0x1C, true), Some(0x58));
        // Keypad 8 vs Up, keypad . vs Delete.
        assert_eq!(usage(0x48, false), Some(0x60));
        assert_eq!(usage(0x48, true), Some(0x52));
        assert_eq!(usage(0x53, false), Some(0x63));
        assert_eq!(usage(0x53, true), Some(0x4C));
        // Windows' Num Lock / Pause reversal.
        assert_eq!(usage(0x45, true), Some(0x53));
        assert_eq!(usage(0x45, false), Some(0x48));
    }

    #[test]
    fn unknown_scan_codes_are_not_guessed() {
        assert_eq!(usage(0x00, false), None);
        assert_eq!(usage(0x54, false), None);
        assert_eq!(usage(0x10, true), None);
        assert_eq!(usage(0xFF, true), None);
    }

    #[test]
    fn every_mapped_usage_is_one_the_protocol_carries_and_no_two_keys_collide() {
        let mut seen = std::collections::BTreeMap::new();
        let mut aliases = Vec::new();
        for extended in [false, true] {
            for scan in 0..=u8::MAX {
                let Some(usage) = usage(scan, extended) else {
                    continue;
                };
                assert!(
                    fr_core::input::PhysicalKey::new(usage).is_some(),
                    "{scan:#04x} {extended} -> {usage:#04x}"
                );
                if let Some(first) = seen.insert(usage, (scan, extended)) {
                    aliases.push((usage, first, (scan, extended)));
                }
            }
        }
        // The one physical key Windows reports two ways: Pause, and the same
        // key under Ctrl (Break).
        assert_eq!(aliases, [(0x48, (0x45, false), (0x46, true))]);
        assert!(seen.len() > 100, "{}", seen.len());
    }
}
