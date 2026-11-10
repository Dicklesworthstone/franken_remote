//! Platform-neutral input rules for the iOS viewer, applied before anything
//! reaches frd's event source (where a refused push ends control).
//!
//! Coordinates arrive in ENCODED PICTURE pixels: the app has already undone
//! its own zoom, pan, letterbox, rotation and safe-area placement. A point
//! outside the picture is refused, never clamped onto an edge. Scrolling
//! accumulates fractional lines and sends only whole lines (the host's
//! granted `LineScroll`), keeping the remainder.

/// The protocol's signed 16.16 line.
pub const LINE: i32 = 1 << 16;
/// Whole lines sent in one scroll event, per axis (frd bounds a scroll action
/// to 32 wheel steps in total).
pub const MAX_LINES_PER_EVENT: i32 = 16;

/// `(x, y)` in 1/256 picture pixels for a point inside the `width` x
/// `height` picture; None outside it or for a non-finite input.
pub fn subpixel_point(picture: (u32, u32), x: f64, y: f64) -> Option<(i64, i64)> {
    let (width, height) = picture;
    if width == 0 || height == 0 || !x.is_finite() || !y.is_finite() {
        return None;
    }
    // Half-open: [0, width) x [0, height). Exactly `width` is outside.
    if x < 0.0 || y < 0.0 || x >= f64::from(width) || y >= f64::from(height) {
        return None;
    }
    // In range by the checks above: at most width * 256 - 1 after flooring.
    #[allow(clippy::cast_possible_truncation)]
    let scale = |v: f64, n: u32| ((v * 256.0).floor() as i64).min(i64::from(n) * 256 - 1);
    Some((scale(x, width), scale(y, height)))
}

/// Fractional line accumulation for touch scrolling. Positive `y` scrolls
/// DOWN (content moves up), positive `x` scrolls RIGHT, as in the protocol.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct LineScroller {
    x: f64,
    y: f64,
}
impl LineScroller {
    /// Add `dx`, `dy` lines; returns whole lines in 16.16 when at least one
    /// line accumulated on some axis. Non-finite input is ignored. Each axis
    /// sends at most [`MAX_LINES_PER_EVENT`] lines; the rest stays queued
    /// in the remainder for the next event, never sent fractionally. The
    /// remainder is bounded at four events' worth per axis: scrolling past
    /// that before the host takes it is dropped (a stuck or runaway
    /// gesture), not queued without limit.
    pub fn add(&mut self, dx: f64, dy: f64) -> Option<(i32, i32)> {
        if dx.is_finite() {
            self.x += dx;
        }
        if dy.is_finite() {
            self.y += dy;
        }
        // Bound a runaway remainder (a stuck gesture) to a few events' worth.
        let cap = f64::from(MAX_LINES_PER_EVENT * 4);
        self.x = self.x.clamp(-cap, cap);
        self.y = self.y.clamp(-cap, cap);
        let whole = |acc: &mut f64| {
            // Gesture deltas are binary fractions of decimal steps: a sum
            // within rounding error of a whole line counts as that line.
            let snapped = if (*acc - acc.round()).abs() < 1e-9 {
                acc.round()
            } else {
                *acc
            };
            let lines = snapped.trunc().clamp(
                -f64::from(MAX_LINES_PER_EVENT),
                f64::from(MAX_LINES_PER_EVENT),
            );
            *acc -= lines;
            // Bounded by the clamp above.
            #[allow(clippy::cast_possible_truncation)]
            let lines = lines as i32;
            lines * LINE
        };
        let (x, y) = (whole(&mut self.x), whole(&mut self.y));
        (x != 0 || y != 0).then_some((x, y))
    }
    /// Forget the remainder (a new gesture, or control ended).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub const fn remainder(&self) -> (f64, f64) {
        (self.x, self.y)
    }
}

/// The protocol's pointer buttons by the app's index (0 primary, 1
/// secondary, 2 middle, 3 back, 4 forward).
pub fn button(index: u32) -> Option<(usize, fr_core::input::PointerButton)> {
    use fr_core::input::PointerButton;
    Some(match index {
        0 => (0, PointerButton::Primary),
        1 => (1, PointerButton::Secondary),
        2 => (2, PointerButton::Middle),
        3 => (3, PointerButton::Back),
        4 => (4, PointerButton::Forward),
        _ => return None,
    })
}

/// Which held-key slot a HID keyboard usage (page 0x07) occupies, when the
/// protocol carries it. `UIKit`'s `UIKeyboardHIDUsage` values are page-7
/// usages already; modifiers are 0xE0..=0xE7.
pub fn key(usage: u16) -> Option<(usize, fr_core::input::PhysicalKey)> {
    let key = fr_core::input::PhysicalKey::new(usage)?;
    Some((usize::from(usage & 0xff), key))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_points_are_half_open_subpixels_and_never_clamped() {
        let p = (1920, 1080);
        assert_eq!(subpixel_point(p, 0.0, 0.0), Some((0, 0)));
        assert_eq!(subpixel_point(p, 960.5, 540.25), Some((245_888, 138_304)));
        assert_eq!(
            subpixel_point(p, 1919.999, 1079.999),
            Some((491_519, 276_479))
        );
        // Outside on every side is refused, not moved onto the edge.
        for (x, y) in [
            (-0.001, 10.0),
            (10.0, -0.5),
            (1920.0, 10.0),
            (10.0, 1080.0),
            (5000.0, 5000.0),
            (f64::NAN, 1.0),
            (1.0, f64::INFINITY),
        ] {
            assert_eq!(subpixel_point(p, x, y), None, "{x},{y}");
        }
        assert_eq!(subpixel_point((0, 10), 0.0, 0.0), None);
    }

    #[test]
    fn scrolling_sends_whole_lines_and_keeps_the_remainder() {
        let mut s = LineScroller::default();
        assert_eq!(s.add(0.0, 0.4), None);
        assert_eq!(s.add(0.0, 0.4), None);
        // 1.2 lines: one whole line goes, 0.2 stays.
        assert_eq!(s.add(0.0, 0.4), Some((0, LINE)));
        assert!((s.remainder().1 - 0.2).abs() < 1e-9);
        // Direction is preserved per axis; mixed signs cancel in the remainder.
        assert_eq!(s.add(-2.5, -1.2), Some((-2 * LINE, -LINE)));
        let (rx, ry) = s.remainder();
        assert!((rx + 0.5).abs() < 1e-9 && ry.abs() < 1e-9, "{rx} {ry}");
        // A huge flick is bounded per event and drained over later events.
        s.reset();
        assert_eq!(s.add(0.0, 40.0), Some((0, MAX_LINES_PER_EVENT * LINE)));
        assert_eq!(s.add(0.0, 0.0), Some((0, MAX_LINES_PER_EVENT * LINE)));
        assert_eq!(s.add(0.0, 0.0), Some((0, 8 * LINE)));
        assert_eq!(s.add(0.0, 0.0), None);
        assert_eq!(s.add(f64::NAN, f64::INFINITY), None);
    }

    #[test]
    fn buttons_and_keys_map_only_known_values() {
        assert!(button(0).is_some() && button(4).is_some());
        assert!(button(5).is_none());
        // HID: 0x04 'a', 0x28 Return, 0xE1 left shift.
        assert_eq!(
            key(0x04).map(|(slot, k)| (slot, k.usage())),
            Some((4, 0x04))
        );
        assert_eq!(
            key(0xE1).map(|(slot, k)| (slot, k.usage())),
            Some((0xE1, 0xE1))
        );
        // Reserved and vendor usages are refused, not passed through.
        for usage in [0, 1, 3, 0xA5, 0xDF, 0xE8, 0x1E0] {
            assert!(key(usage).is_none(), "{usage:#x}");
        }
    }
}
