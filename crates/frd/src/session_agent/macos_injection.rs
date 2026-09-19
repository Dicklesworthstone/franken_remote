//! macOS keyboard/pointer injection adapter with submission-time checkpoints (plan §§5.3, 7.3, 15.2).
//!
//! Posts CGEvents under the macOS Accessibility permission (`kTCCServiceAccessibility`).
//! Enforces:
//! 1. Submission-time Accessibility permission check: typed refusal if permission missing.
//! 2. Separation of physical key identity from committed text (plan §15.2):
//!    - Physical keys map USB HID usage (page 0x07) to macOS virtual keycodes.
//!    - Committed text uses the qualified Unicode text path, never synthesized keycodes.
//! 3. Desktop coordinate bounds validation.

use fr_core::{
    input::{DesktopPoint, InputBounds, KeyTransition, PhysicalKey, PointerButton, ScrollUnit},
    input_submission::{Capabilities, Capability, InputSink, Operation, PlatformError, Submission},
};

/// macOS virtual keycode (kVK_*).
pub type MacOsKeyCode = u16;

/// Map USB HID usage (page 0x07) to macOS virtual keycode.
pub fn hid_to_macos_keycode(usage: u16) -> Option<MacOsKeyCode> {
    match usage {
        0x04 => Some(0x00), // A
        0x05 => Some(0x0B), // B
        0x06 => Some(0x08), // C
        0x07 => Some(0x02), // D
        0x08 => Some(0x0E), // E
        0x09 => Some(0x03), // F
        0x0A => Some(0x05), // G
        0x0B => Some(0x04), // H
        0x0C => Some(0x22), // I
        0x0D => Some(0x26), // J
        0x0E => Some(0x28), // K
        0x0F => Some(0x25), // L
        0x10 => Some(0x2E), // M
        0x11 => Some(0x2D), // N
        0x12 => Some(0x1F), // O
        0x13 => Some(0x23), // P
        0x14 => Some(0x0C), // Q
        0x15 => Some(0x0F), // R
        0x16 => Some(0x01), // S
        0x17 => Some(0x11), // T
        0x18 => Some(0x20), // U
        0x19 => Some(0x09), // V
        0x1A => Some(0x0D), // W
        0x1B => Some(0x07), // X
        0x1C => Some(0x10), // Y
        0x1D => Some(0x06), // Z

        0x1E => Some(0x12), // 1
        0x1F => Some(0x13), // 2
        0x20 => Some(0x14), // 3
        0x21 => Some(0x15), // 4
        0x22 => Some(0x17), // 5
        0x23 => Some(0x16), // 6
        0x24 => Some(0x1A), // 7
        0x25 => Some(0x1C), // 8
        0x26 => Some(0x19), // 9
        0x27 => Some(0x1D), // 0

        0x28 => Some(0x24), // Return
        0x29 => Some(0x35), // Escape
        0x2A => Some(0x33), // Delete / Backspace
        0x2B => Some(0x30), // Tab
        0x2C => Some(0x31), // Spacebar
        0x2D => Some(0x1B), // - / _
        0x2E => Some(0x18), // = / +
        0x2F => Some(0x21), // [ / {
        0x30 => Some(0x1E), // ] / }
        0x31 => Some(0x2A), // \ / |
        0x33 => Some(0x29), // ; / :
        0x34 => Some(0x27), // ' / "
        0x35 => Some(0x32), // ` / ~
        0x36 => Some(0x2B), // , / <
        0x37 => Some(0x2F), // . / >
        0x38 => Some(0x2C), // / / ?
        0x39 => Some(0x39), // Caps Lock

        0x3A => Some(0x7A), // F1
        0x3B => Some(0x78), // F2
        0x3C => Some(0x63), // F3
        0x3D => Some(0x76), // F4
        0x3E => Some(0x60), // F5
        0x3F => Some(0x61), // F6
        0x40 => Some(0x62), // F7
        0x41 => Some(0x64), // F8
        0x42 => Some(0x65), // F9
        0x43 => Some(0x6D), // F10
        0x44 => Some(0x67), // F11
        0x45 => Some(0x6F), // F12

        0x4F => Some(0x7C), // Right Arrow
        0x50 => Some(0x7B), // Left Arrow
        0x51 => Some(0x7D), // Down Arrow
        0x52 => Some(0x7E), // Up Arrow

        0xE0 => Some(0x3B), // Left Control
        0xE1 => Some(0x38), // Left Shift
        0xE2 => Some(0x3A), // Left Option (Alt)
        0xE3 => Some(0x37), // Left Command (GUI)
        0xE4 => Some(0x3E), // Right Control
        0xE5 => Some(0x3C), // Right Shift
        0xE6 => Some(0x3D), // Right Option (Alt)
        0xE7 => Some(0x36), // Right Command (GUI)

        _ => None,
    }
}

/// A dispatched CGEvent representation for posting and inspection.
#[derive(Debug, Clone, PartialEq)]
pub enum PostedCgEvent {
    /// Keyboard physical key event posted via CGEventCreateKeyboardEvent.
    Key {
        keycode: MacOsKeyCode,
        down: bool,
    },
    /// Unicode committed text posted via CGEventKeyboardSetUnicodeString.
    /// This is strictly distinct from physical key events (plan §15.2).
    Text {
        character: char,
    },
    /// Pointer movement event (kCGEventMouseMoved).
    MouseMove {
        x: f64,
        y: f64,
    },
    /// Pointer button event (LeftMouseDown, RightMouseDown, OtherMouseDown, etc.).
    MouseButton {
        button: u32,
        down: bool,
        x: f64,
        y: f64,
    },
    /// Scroll event (kCGEventScrollWheel).
    Scroll {
        dx: i32,
        dy: i32,
        is_continuous: bool,
    },
}

/// Abstract contract for posting CGEvents to the macOS window server.
pub trait MacOsEventPoster: Send {
    /// Check whether the process holds macOS Accessibility permission (`kTCCServiceAccessibility`).
    fn has_accessibility_permission(&self) -> bool;
    /// Post a synthetic CGEvent.
    fn post_event(&mut self, event: PostedCgEvent) -> Result<(), PlatformError>;
}

/// Test/Mock event poster that records all posted CGEvents for verification.
#[derive(Default)]
pub struct RecordingPoster {
    pub has_permission: bool,
    pub events: Vec<PostedCgEvent>,
}

impl RecordingPoster {
    pub fn new(has_permission: bool) -> Self {
        Self {
            has_permission,
            events: Vec::new(),
        }
    }
}

impl MacOsEventPoster for RecordingPoster {
    fn has_accessibility_permission(&self) -> bool {
        self.has_permission
    }

    fn post_event(&mut self, event: PostedCgEvent) -> Result<(), PlatformError> {
        if !self.has_permission {
            return Err(PlatformError::Permission);
        }
        self.events.push(event);
        Ok(())
    }
}

/// macOS input sink implementing `InputSink` with strict submission-time checks.
pub struct MacOsInputSink<P: MacOsEventPoster> {
    poster: P,
    bounds: InputBounds,
    capabilities: Capabilities,
    cursor_pos: DesktopPoint,
    prepared: Option<Operation>,
}

impl<P: MacOsEventPoster> MacOsInputSink<P> {
    pub fn new(poster: P, bounds: InputBounds) -> Self {
        let capabilities = Capabilities::default()
            .with(Capability::Keys)
            .with(Capability::Repeat)
            .with(Capability::Absolute)
            .with(Capability::Buttons)
            .with(Capability::LineScroll)
            .with(Capability::PixelScroll)
            .with(Capability::Text);

        Self {
            poster,
            bounds,
            capabilities,
            cursor_pos: DesktopPoint::new(0, 0),
            prepared: None,
        }
    }

    pub fn poster(&self) -> &P {
        &self.poster
    }

    pub fn poster_mut(&mut self) -> &mut P {
        &mut self.poster
    }

    pub fn bounds(&self) -> InputBounds {
        self.bounds
    }

    pub fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Submission checkpoint check: verifies permission and bounds before native posting.
    fn verify_submission(&self, op: &Operation) -> Result<(), PlatformError> {
        // 1. Strict Accessibility permission check immediately before submission
        if !self.poster.has_accessibility_permission() {
            return Err(PlatformError::Permission);
        }

        // 2. Bounds check for coordinates
        match op {
            Operation::Absolute(pt) => {
                if !self.bounds.contains(*pt) {
                    return Err(PlatformError::GeometryChanged);
                }
            }
            Operation::Key { key, .. } => {
                if hid_to_macos_keycode(key.usage()).is_none() {
                    return Err(PlatformError::Unsupported);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

impl<P: MacOsEventPoster> InputSink for MacOsInputSink<P> {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        self.verify_submission(&operation)?;
        self.prepared = Some(operation);
        Ok(())
    }

    fn submit(&mut self, operation: Operation) -> Submission {
        if let Err(e) = self.verify_submission(&operation) {
            self.prepared = None;
            return Submission::NotSubmitted(e);
        }

        let event = match operation {
            Operation::Key { key, transition } => {
                let Some(keycode) = hid_to_macos_keycode(key.usage()) else {
                    return Submission::NotSubmitted(PlatformError::Unsupported);
                };
                let down = match transition {
                    KeyTransition::Press | KeyTransition::Repeat => true,
                    KeyTransition::Release => false,
                };
                PostedCgEvent::Key { keycode, down }
            }
            Operation::Text(character) => {
                // Qualified committed text path, distinct from physical keycodes!
                PostedCgEvent::Text { character }
            }
            Operation::Absolute(pt) => {
                self.cursor_pos = pt;
                PostedCgEvent::MouseMove {
                    x: f64::from(pt.x),
                    y: f64::from(pt.y),
                }
            }
            Operation::Button { button, pressed } => {
                let btn_num = match button {
                    PointerButton::Primary => 0,
                    PointerButton::Secondary => 1,
                    PointerButton::Middle => 2,
                    PointerButton::Back => 3,
                    PointerButton::Forward => 4,
                };
                PostedCgEvent::MouseButton {
                    button: btn_num,
                    down: pressed,
                    x: f64::from(self.cursor_pos.x),
                    y: f64::from(self.cursor_pos.y),
                }
            }
            Operation::Scroll { x, y, unit } => {
                let is_continuous = matches!(unit, ScrollUnit::Pixels);
                PostedCgEvent::Scroll {
                    dx: x,
                    dy: y,
                    is_continuous,
                }
            }
            Operation::Relative { .. } | Operation::Wheel { .. } => {
                return Submission::NotSubmitted(PlatformError::Unsupported);
            }
        };

        let result = self.poster.post_event(event);
        self.prepared = None;
        match result {
            Ok(()) => Submission::Submitted,
            Err(e) => Submission::NotSubmitted(e),
        }
    }

    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
}
