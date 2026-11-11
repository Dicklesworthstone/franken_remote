//! The macOS input executor's pure bookkeeping (fr-macos-host-control-b14s):
//! the keys and buttons it holds, the modifier flags its events carry, a
//! press's click count and the Quartz form of scroll distances. No Quartz call
//! here, so ordinary CI tests it on every target.
#![forbid(unsafe_code)]

use crate::macos_keys;
use fr_core::{
    input::PointerButton,
    input_submission::{
        PlatformError,
        scroll::{LINE, LineScroll},
    },
};
use std::time::{Duration, Instant};

/// Virtual keycodes 0..=127 and the five buttons an executor holds. No Debug:
/// which keys are held is input content.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Held {
    keys: [u64; 2],
    buttons: u8,
}

impl Held {
    pub fn key(&self, keycode: u16) -> bool {
        keycode < 128 && self.keys[usize::from(keycode / 64)] & (1 << (keycode % 64)) != 0
    }
    pub fn set_key(&mut self, keycode: u16, down: bool) {
        if keycode >= 128 {
            return;
        }
        let (word, bit) = (usize::from(keycode / 64), 1 << (keycode % 64));
        if down {
            self.keys[word] |= bit;
        } else {
            self.keys[word] &= !bit;
        }
    }
    pub const fn button(&self, index: usize) -> bool {
        index < 5 && self.buttons & (1 << index) != 0
    }
    pub fn set_button(&mut self, index: usize, down: bool) {
        if index >= 5 {
            return;
        }
        if down {
            self.buttons |= 1 << index;
        } else {
            self.buttons &= !(1 << index);
        }
    }
    pub fn keys(&self) -> impl Iterator<Item = u16> + '_ {
        (0..128).filter(|&keycode| self.key(keycode))
    }
    pub fn buttons(&self) -> impl Iterator<Item = usize> + '_ {
        (0..5).filter(|&index| self.button(index))
    }
    pub const fn is_empty(&self) -> bool {
        self.keys[0] == 0 && self.keys[1] == 0 && self.buttons == 0
    }
    /// The words the last-resort release reads: keys 0..64, 64..128, buttons.
    pub fn words(&self) -> (u64, u64, u32) {
        (self.keys[0], self.keys[1], u32::from(self.buttons))
    }
    /// Both sets together: the executor's pessimistic release set includes a
    /// press it has prepared but not yet posted.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self {
            keys: [self.keys[0] | other.keys[0], self.keys[1] | other.keys[1]],
            buttons: self.buttons | other.buttons,
        }
    }
    /// Quartz event flags while these keys are held: each held modifier's
    /// device-independent flag and its left/right device bit, as hardware
    /// reports them, so Command-click and Shift-drag reach applications.
    pub fn flags(&self) -> u64 {
        self.keys()
            .filter_map(macos_keys::modifier)
            .fold(0, |flags, m| flags | m.flag | m.device_mask)
    }
    /// The lowest-numbered held button: a move while it is down is a drag.
    pub fn dragging(&self) -> Option<usize> {
        self.buttons().next()
    }
}

/// Quartz needs a virtual key even for an explicitly supplied Unicode value.
/// Text uses this key only while the remote held set is empty; its key-up is
/// release-only cleanup, never a second committed character.
pub const TEXT_KEYCODE: u16 = 0;

/// One complete Unicode scalar, prepared before the final authority check.
/// A supplementary scalar's surrogate pair is one native string, not two
/// independently cancellable operations. No Debug: this is typed content.
pub struct CommittedScalar {
    units: [u16; 2],
    length: usize,
}

impl CommittedScalar {
    pub fn new(scalar: char, held: Held) -> Result<Self, PlatformError> {
        // Do not turn text into a shortcut or release an already-held key.
        // The caller can explicitly release its physical state and submit a
        // NEW request; this operation never changes or replays that state.
        if !held.is_empty() {
            return Err(PlatformError::Permission);
        }
        let mut units = [0; 2];
        let length = scalar.encode_utf16(&mut units).len();
        Ok(Self { units, length })
    }

    pub fn units(&self) -> &[u16] {
        &self.units[..self.length]
    }
}

/// The Quartz button number: 0 left, 1 right, 2 middle, 3 back, 4 forward.
pub const fn button_index(button: PointerButton) -> usize {
    match button {
        PointerButton::Primary => 0,
        PointerButton::Secondary => 1,
        PointerButton::Middle => 2,
        PointerButton::Back => 3,
        PointerButton::Forward => 4,
    }
}

/// Farthest (points) a press may be from the previous one and still continue
/// its click sequence, like the window server's own click space.
const CLICK_SLOP: f64 = 4.0;

/// Click counting for posted presses: Quartz trusts the click state an event
/// carries, so a double click is two presses of one button within the
/// double-click interval and a few points of each other.
#[derive(Clone)]
pub struct Clicks {
    interval: Duration,
    last: Option<(usize, Instant, (f64, f64), i64)>,
    pressed: [i64; 5],
}

impl Clicks {
    pub const fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: None,
            pressed: [1; 5],
        }
    }
    /// The click state of a press of `button` at `point`, `now`.
    pub fn press(&mut self, button: usize, now: Instant, point: (f64, f64)) -> i64 {
        let clicks = match self.last {
            Some((previous, at, (x, y), count))
                if previous == button
                    && now
                        .checked_duration_since(at)
                        .is_some_and(|d| d <= self.interval)
                    && (point.0 - x).hypot(point.1 - y) <= CLICK_SLOP =>
            {
                count.saturating_add(1)
            }
            _ => 1,
        };
        self.last = Some((button, now, point, clicks));
        if let Some(slot) = self.pressed.get_mut(button) {
            *slot = clicks;
        }
        clicks
    }
    /// A release carries its press's click state.
    pub fn release(&self, button: usize) -> i64 {
        self.pressed.get(button).copied().unwrap_or(1)
    }
}

/// A protocol line scroll (signed 16.16, y < 0 up, x < 0 left) as Quartz
/// wheel counts `(vertical, horizontal)`, positive up and left. Exactly the
/// X11 host's rule: whole lines only, at most `MAX_STEPS` in one action;
/// fractions are refused, never rounded or accumulated.
pub fn scroll_lines(x: i32, y: i32) -> Option<(i32, i32)> {
    LineScroll::new(x, y)?;
    Some((-(y / LINE), -(x / LINE)))
}

/// Protocol logical pixel distances as Quartz signed 16.16 distances,
/// `(vertical, horizontal)`. No line conversion or display backing scale.
/// The one unrepresentable sign inversion is refused before any OS call.
pub const fn scroll_pixels(x: i32, y: i32) -> Option<(i32, i32)> {
    match (y.checked_neg(), x.checked_neg()) {
        (Some(vertical), Some(horizontal)) => Some((vertical, horizontal)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_scalars_preserve_bmp_supplementary_and_boundary_values() {
        for (scalar, expected) in [
            ('\0', &[0][..]),
            ('é', &[0x00e9][..]),
            ('\u{d7ff}', &[0xd7ff][..]),
            ('\u{e000}', &[0xe000][..]),
            ('\u{ffff}', &[0xffff][..]),
            ('\u{10000}', &[0xd800, 0xdc00][..]),
            ('🙂', &[0xd83d, 0xde42][..]),
            ('\u{10ffff}', &[0xdbff, 0xdfff][..]),
        ] {
            let prepared = CommittedScalar::new(scalar, Held::default()).unwrap();
            assert_eq!(prepared.units(), expected);
            assert_eq!(
                char::decode_utf16(prepared.units().iter().copied()).count(),
                1
            );
        }
    }

    #[test]
    fn committed_text_never_borrows_a_held_key_modifier_or_button() {
        for key in [TEXT_KEYCODE, 0x0b, 0x37, 0x38, 0x3c, 127] {
            let mut held = Held::default();
            held.set_key(key, true);
            assert!(matches!(
                CommittedScalar::new('é', held),
                Err(PlatformError::Permission)
            ));
            assert!(held.key(key));
        }
        for button in 0..5 {
            let mut held = Held::default();
            held.set_button(button, true);
            assert!(matches!(
                CommittedScalar::new('🙂', held),
                Err(PlatformError::Permission)
            ));
            assert!(held.button(button));
        }
    }

    #[test]
    fn pixel_distances_keep_fractional_units_and_refuse_overflow() {
        assert_eq!(
            scroll_pixels(3 * LINE / 2, -9 * LINE / 4),
            Some((9 * LINE / 4, -3 * LINE / 2))
        );
        assert_eq!(scroll_pixels(1, -1), Some((1, -1)));
        assert_eq!(
            scroll_pixels(i32::MAX, -i32::MAX),
            Some((i32::MAX, -i32::MAX))
        );
        assert_eq!(scroll_pixels(i32::MIN, 0), None);
        assert_eq!(scroll_pixels(0, i32::MIN), None);
    }

    #[test]
    fn modifier_flags_follow_the_held_keys() {
        let mut held = Held::default();
        assert_eq!(held.flags(), 0);
        held.set_key(0x00, true); // A: not a modifier
        assert_eq!(held.flags(), 0);
        held.set_key(0x37, true); // left Command
        assert_eq!(held.flags(), (1 << 20) | 0x0008);
        held.set_key(0x3c, true); // right Shift
        assert_eq!(held.flags(), (1 << 20) | 0x0008 | (1 << 17) | 0x0004);
        held.set_key(0x37, false);
        assert_eq!(held.flags(), (1 << 17) | 0x0004);
        // Caps Lock toggles; it is never a held flag.
        held.set_key(0x39, true);
        assert_eq!(held.flags(), (1 << 17) | 0x0004);
    }

    #[test]
    fn held_sets_cover_every_keycode_and_button_once() {
        let mut held = Held::default();
        assert!(held.is_empty());
        for keycode in [0, 63, 64, 127] {
            held.set_key(keycode, true);
        }
        held.set_key(128, true);
        assert_eq!(held.keys().collect::<Vec<_>>(), [0, 63, 64, 127]);
        assert_eq!(held.words(), (1 | 1 << 63, 1 | 1 << 63, 0));
        held.set_button(4, true);
        held.set_button(1, true);
        held.set_button(5, true);
        assert_eq!(held.buttons().collect::<Vec<_>>(), [1, 4]);
        assert_eq!(held.dragging(), Some(1));
        let mut pressing = Held::default();
        pressing.set_button(0, true);
        assert_eq!(held.union(pressing).dragging(), Some(0));
        for keycode in [0, 63, 64, 127] {
            held.set_key(keycode, false);
        }
        held.set_button(1, false);
        held.set_button(4, false);
        assert!(held.is_empty());
    }

    #[test]
    fn buttons_use_quartz_numbers() {
        assert_eq!(button_index(PointerButton::Primary), 0);
        assert_eq!(button_index(PointerButton::Secondary), 1);
        assert_eq!(button_index(PointerButton::Middle), 2);
        assert_eq!(button_index(PointerButton::Back), 3);
        assert_eq!(button_index(PointerButton::Forward), 4);
    }

    #[test]
    fn a_double_click_is_two_close_presses_of_one_button() {
        let mut clicks = Clicks::new(Duration::from_millis(500));
        let t0 = Instant::now();
        assert_eq!(clicks.press(0, t0, (100.0, 100.0)), 1);
        assert_eq!(clicks.release(0), 1);
        let t1 = t0 + Duration::from_millis(200);
        assert_eq!(clicks.press(0, t1, (101.5, 99.0)), 2);
        assert_eq!(clicks.release(0), 2);
        let t2 = t1 + Duration::from_millis(200);
        assert_eq!(clicks.press(0, t2, (101.0, 100.0)), 3, "triple click");
        // Too late, too far, or another button: a new sequence.
        let t3 = t2 + Duration::from_millis(501);
        assert_eq!(clicks.press(0, t3, (101.0, 100.0)), 1);
        assert_eq!(clicks.press(0, t3, (110.0, 100.0)), 1);
        assert_eq!(clicks.press(1, t3, (110.0, 100.0)), 1);
        assert_eq!(clicks.release(1), 1);
        assert_eq!(clicks.release(0), 1);
    }

    #[test]
    fn line_scrolls_are_whole_bounded_and_flipped_to_quartz() {
        // Protocol: y < 0 is up, x < 0 is left. Quartz: positive is up, left.
        assert_eq!(scroll_lines(0, -LINE), Some((1, 0)));
        assert_eq!(scroll_lines(0, 3 * LINE), Some((-3, 0)));
        assert_eq!(scroll_lines(-2 * LINE, 0), Some((0, 2)));
        assert_eq!(scroll_lines(LINE, LINE), Some((-1, -1)));
        assert_eq!(scroll_lines(0, LINE / 2), None, "fractions are refused");
        assert_eq!(scroll_lines(33 * LINE, 0), None, "beyond MAX_STEPS");
        assert_eq!(scroll_lines(i32::MIN, 0), None);
    }
}
