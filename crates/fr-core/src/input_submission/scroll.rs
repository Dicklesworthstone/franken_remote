//! Bounded discrete-wheel expansion of the existing signed 16.16 line units.
//! Fractional lines are not rounded, accumulated or converted from pixel input.

/// One line in the protocol's signed 16.16 distance representation.
pub const LINE: i32 = 1 << 16;
/// Bound native effects per input action: position plus at most 64 transitions.
pub const MAX_STEPS: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelDirection {
    Left,
    Right,
    Up,
    Down,
}

/// No payload Debug: direction and distance are input, not diagnostics.
#[derive(Clone, Copy)]
pub struct LineScroll {
    horizontal: u32,
    vertical: u32,
    x: WheelDirection,
    y: WheelDirection,
}
impl LineScroll {
    /// Only an exact, bounded number of whole lines has a discrete realization.
    /// Cast after divisibility/range checks; `i32::MIN` is never negated.
    pub fn new(x: i32, y: i32) -> Option<Self> {
        if x % LINE != 0 || y % LINE != 0 {
            return None;
        }
        let horizontal = (x / LINE).unsigned_abs();
        let vertical = (y / LINE).unsigned_abs();
        if horizontal.checked_add(vertical)? > MAX_STEPS {
            return None;
        }
        Some(Self {
            horizontal,
            vertical,
            x: if x < 0 {
                WheelDirection::Left
            } else {
                WheelDirection::Right
            },
            y: if y < 0 {
                WheelDirection::Up
            } else {
                WheelDirection::Down
            },
        })
    }
    /// Number of API submissions for a complete compound action, including its
    /// preceding absolute pointer position. Not application effects or lines.
    pub const fn native_operations(self) -> u32 {
        1 + 2 * (self.horizontal + self.vertical)
    }
    /// Horizontal then vertical, in the same reliable action and original ticket.
    pub fn steps(self) -> impl Iterator<Item = WheelDirection> {
        std::iter::repeat_n(self.x, self.horizontal as usize)
            .chain(std::iter::repeat_n(self.y, self.vertical as usize))
    }
}
