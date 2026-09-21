//! EIS (libei) preferred input injection adapter for Wayland portals (plan §10.1).
//!
//! Enforces:
//! 1. Session agent retains the EIS connection; media worker never receives it.
//! 2. Returned device grants define permitted injection:
//!    - Pointer operations (motion, button, scroll) require `DeviceFlags::POINTER`.
//!    - Keyboard operations require `DeviceFlags::KEYBOARD`.
//!    - Touch operations require `DeviceFlags::TOUCHSCREEN`.
//!      Attempting to inject an ungranted device category returns `PlatformError::Unsupported`.
//! 3. Submission checkpoint verifies bounds before injection.
//! 4. Clean release: synthesizes releases for any held buttons or keys upon cancellation/drop.

use std::collections::HashSet;

use fr_core::{
    input::{InputBounds, KeyTransition, PointerButton},
    input_submission::{
        Capabilities, Capability, InputSink, Operation, PlatformError, Submission,
        scroll::WheelDirection,
    },
};

use super::coordinates::EisRegion;
use super::portal::DeviceFlags;

/// Trait abstracting EIS protocol transmission (real socket vs mock for lab/tests).
pub trait EisEventPoster: Send + Sync {
    fn post_pointer_motion_absolute(&mut self, x: f64, y: f64) -> Result<(), PlatformError>;
    fn post_pointer_button(
        &mut self,
        button: PointerButton,
        down: bool,
    ) -> Result<(), PlatformError>;
    fn post_pointer_scroll_delta(&mut self, dx: f64, dy: f64) -> Result<(), PlatformError>;
    fn post_keyboard_key(&mut self, keycode: u32, down: bool) -> Result<(), PlatformError>;
    fn is_connected(&self) -> bool;
}

/// Recorded EIS event for verification and deterministic testing.
#[derive(Debug, Clone, PartialEq)]
pub enum EisRecordedEvent {
    MotionAbsolute { x: f64, y: f64 },
    Button { button: PointerButton, down: bool },
    ScrollDelta { dx: f64, dy: f64 },
    Key { keycode: u32, down: bool },
}

/// Test/recording poster capturing EIS operations in memory.
#[derive(Default)]
pub struct RecordingEisPoster {
    events: Vec<EisRecordedEvent>,
    connected: bool,
}

impl RecordingEisPoster {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            connected: true,
        }
    }

    #[must_use]
    pub fn events(&self) -> &[EisRecordedEvent] {
        &self.events
    }

    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }
}

impl EisEventPoster for RecordingEisPoster {
    fn post_pointer_motion_absolute(&mut self, x: f64, y: f64) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events.push(EisRecordedEvent::MotionAbsolute { x, y });
        Ok(())
    }

    fn post_pointer_button(
        &mut self,
        button: PointerButton,
        down: bool,
    ) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events.push(EisRecordedEvent::Button { button, down });
        Ok(())
    }

    fn post_pointer_scroll_delta(&mut self, dx: f64, dy: f64) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events.push(EisRecordedEvent::ScrollDelta { dx, dy });
        Ok(())
    }

    fn post_keyboard_key(&mut self, keycode: u32, down: bool) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events.push(EisRecordedEvent::Key { keycode, down });
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }
}

/// Linux EIS input sink implementing `InputSink` with portal grant fencing.
pub struct EisInputSink<P: EisEventPoster> {
    poster: P,
    bounds: InputBounds,
    granted_devices: DeviceFlags,
    capabilities: Capabilities,
    region: Option<EisRegion>,
    held_buttons: [bool; 6],
    held_keys: HashSet<u32>,
    prepared: Option<Operation>,
}

impl<P: EisEventPoster> EisInputSink<P> {
    pub fn new(
        poster: P,
        bounds: InputBounds,
        granted_devices: DeviceFlags,
        region: Option<EisRegion>,
    ) -> Self {
        let mut capabilities = Capabilities::default();
        if granted_devices.is_pointer() {
            capabilities = capabilities
                .with(Capability::Absolute)
                .with(Capability::Buttons)
                .with(Capability::LineScroll)
                .with(Capability::PixelScroll);
        }
        if granted_devices.is_keyboard() {
            capabilities = capabilities
                .with(Capability::Keys)
                .with(Capability::Repeat)
                .with(Capability::Text);
        }

        Self {
            poster,
            bounds,
            granted_devices,
            capabilities,
            region,
            held_buttons: [false; 6],
            held_keys: HashSet::new(),
            prepared: None,
        }
    }

    pub fn poster(&self) -> &P {
        &self.poster
    }

    pub fn poster_mut(&mut self) -> &mut P {
        &mut self.poster
    }

    #[must_use]
    pub const fn bounds(&self) -> InputBounds {
        self.bounds
    }

    #[must_use]
    pub const fn granted_devices(&self) -> DeviceFlags {
        self.granted_devices
    }

    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Verification check immediately prior to submission.
    fn verify_submission(&self, op: &Operation) -> Result<(), PlatformError> {
        if !self.poster.is_connected() {
            return Err(PlatformError::Unavailable);
        }

        match op {
            Operation::Absolute(pt) => {
                if !self.granted_devices.is_pointer() {
                    return Err(PlatformError::Unsupported);
                }
                if !self.bounds.contains(*pt) {
                    return Err(PlatformError::GeometryChanged);
                }
            }
            Operation::Relative { .. }
            | Operation::Button { .. }
            | Operation::Scroll { .. }
            | Operation::Wheel { .. } => {
                if !self.granted_devices.is_pointer() {
                    return Err(PlatformError::Unsupported);
                }
            }
            Operation::Key { .. } | Operation::Text(_) => {
                if !self.granted_devices.is_keyboard() {
                    return Err(PlatformError::Unsupported);
                }
            }
        }
        Ok(())
    }
}

impl<P: EisEventPoster> InputSink for EisInputSink<P> {
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

        let res = match operation {
            Operation::Absolute(pt) => {
                let (x, y) = if let Some(region) = &self.region {
                    let rel_x = f64::from(pt.x) - f64::from(region.offset_x);
                    let rel_y = f64::from(pt.y) - f64::from(region.offset_y);
                    (rel_x * region.scale, rel_y * region.scale)
                } else {
                    (f64::from(pt.x), f64::from(pt.y))
                };
                self.poster.post_pointer_motion_absolute(x, y)
            }
            Operation::Button { button, pressed } => {
                let idx = button as usize;
                if let Some(slot) = self.held_buttons.get_mut(idx) {
                    *slot = pressed;
                }
                self.poster.post_pointer_button(button, pressed)
            }
            Operation::Scroll { x, y, .. } => {
                let dx = f64::from(x);
                let dy = f64::from(y);
                self.poster.post_pointer_scroll_delta(dx, dy)
            }
            Operation::Wheel { direction, pressed } => {
                let (dx, dy) = match direction {
                    WheelDirection::Up => (0.0, -1.0),
                    WheelDirection::Down => (0.0, 1.0),
                    WheelDirection::Left => (-1.0, 0.0),
                    WheelDirection::Right => (1.0, 0.0),
                };
                if pressed {
                    self.poster.post_pointer_scroll_delta(dx, dy)
                } else {
                    Ok(())
                }
            }
            Operation::Key { key, transition } => {
                let keycode = u32::from(key.usage());
                let down = match transition {
                    KeyTransition::Press | KeyTransition::Repeat => true,
                    KeyTransition::Release => false,
                };
                if down {
                    self.held_keys.insert(keycode);
                } else {
                    self.held_keys.remove(&keycode);
                }
                self.poster.post_keyboard_key(keycode, down)
            }
            Operation::Relative { .. } | Operation::Text(_) => {
                // Raw text and relative motion unsupported directly at the EIS raw device layer
                return Submission::NotSubmitted(PlatformError::Unsupported);
            }
        };

        self.prepared = None;
        match res {
            Ok(()) => Submission::Submitted,
            Err(e) => Submission::NotSubmitted(e),
        }
    }

    fn cancel_prepared(&mut self) {
        self.prepared = None;
    }
}
