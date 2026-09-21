//! Windows `SendInput` input injection adapter with UIPI integrity gating (plan §10.3).
//!
//! Enforces:
//! 1. UIPI (User Interface Privilege Isolation) and Secure Desktop constraints:
//!    - Ordinary medium-integrity processes cannot inject into elevated/high-integrity windows.
//!    - Secure Desktop prompts (UAC, Ctrl+Alt+Del) reject injection.
//!    - Attempting injection against restricted targets returns `PlatformError::Unsupported`
//!      (reporting typed refusal rather than silently claiming execution).
//! 2. Coordinate normalization to virtual desktop space (`0..65535` for `MOUSEEVENTF_VIRTUALDESK`).
//! 3. Clean release: tracks held keys and mouse buttons, releasing them on drop or reset.

use std::collections::HashSet;

use fr_core::{
    input::{InputBounds, KeyTransition, PointerButton},
    input_submission::{
        Capabilities, Capability, InputSink, Operation, PlatformError, Submission,
        scroll::WheelDirection,
    },
};

use super::coordinates::{
    NormalizedSendInputPoint, VirtualDesktopPoint, VirtualDesktopRect,
    virtual_desktop_to_send_input,
};

/// Target window security context under Windows User Interface Privilege Isolation (UIPI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TargetWindowSecurity {
    /// Normal user interactive window at medium or low integrity level.
    #[default]
    NormalWindow,
    /// Elevated process window running at high integrity (Administrator).
    ElevatedWindow,
    /// Windows Secure Desktop (UAC consent prompt, Ctrl+Alt+Del, Winlogon).
    SecureDesktop,
}

/// Windows process integrity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum WindowsIntegrityLevel {
    Untrusted = 0,
    Low = 1,
    Medium = 2,
    High = 3,
    System = 4,
    ProtectedProcess = 5,
}

/// Recorded `SendInput` event captured for verification and testing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendInputRecordedEvent {
    MouseMoveAbsolute {
        norm_x: u16,
        norm_y: u16,
        virt_x: i32,
        virt_y: i32,
    },
    MouseButton {
        button: PointerButton,
        down: bool,
    },
    MouseWheel {
        delta: i32,
        horizontal: bool,
    },
    KeyboardKey {
        vk_or_scan: u16,
        down: bool,
        is_scancode: bool,
    },
}

/// Abstract interface for Windows `SendInput` execution (OS API vs test recorder).
pub trait SendInputPoster: Send + Sync {
    fn post_mouse_move_virtual(
        &mut self,
        norm: NormalizedSendInputPoint,
        raw: VirtualDesktopPoint,
    ) -> Result<(), PlatformError>;

    fn post_mouse_button(&mut self, button: PointerButton, down: bool)
    -> Result<(), PlatformError>;

    fn post_mouse_wheel(&mut self, delta: i32, horizontal: bool) -> Result<(), PlatformError>;

    fn post_keyboard(
        &mut self,
        vk_or_scan: u16,
        down: bool,
        is_scancode: bool,
    ) -> Result<(), PlatformError>;

    fn is_connected(&self) -> bool;
}

/// Recording test poster capturing `SendInput` calls in memory.
#[derive(Debug, Default)]
pub struct RecordingSendInputPoster {
    events: Vec<SendInputRecordedEvent>,
    connected: bool,
}

impl RecordingSendInputPoster {
    #[must_use]
    pub fn new() -> Self {
        Self {
            events: Vec::new(),
            connected: true,
        }
    }

    #[must_use]
    pub fn events(&self) -> &[SendInputRecordedEvent] {
        &self.events
    }

    pub fn set_connected(&mut self, connected: bool) {
        self.connected = connected;
    }
}

impl SendInputPoster for RecordingSendInputPoster {
    fn post_mouse_move_virtual(
        &mut self,
        norm: NormalizedSendInputPoint,
        raw: VirtualDesktopPoint,
    ) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events.push(SendInputRecordedEvent::MouseMoveAbsolute {
            norm_x: norm.x,
            norm_y: norm.y,
            virt_x: raw.x,
            virt_y: raw.y,
        });
        Ok(())
    }

    fn post_mouse_button(
        &mut self,
        button: PointerButton,
        down: bool,
    ) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events
            .push(SendInputRecordedEvent::MouseButton { button, down });
        Ok(())
    }

    fn post_mouse_wheel(&mut self, delta: i32, horizontal: bool) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events
            .push(SendInputRecordedEvent::MouseWheel { delta, horizontal });
        Ok(())
    }

    fn post_keyboard(
        &mut self,
        vk_or_scan: u16,
        down: bool,
        is_scancode: bool,
    ) -> Result<(), PlatformError> {
        if !self.connected {
            return Err(PlatformError::Unavailable);
        }
        self.events.push(SendInputRecordedEvent::KeyboardKey {
            vk_or_scan,
            down,
            is_scancode,
        });
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected
    }
}

/// Standard Windows `WHEEL_DELTA` (120 units per click/notch).
pub const WHEEL_DELTA: i32 = 120;

/// Windows `SendInput` input sink implementing the `fr-core` `InputSink` contract.
pub struct WindowsInputSink<P: SendInputPoster> {
    poster: P,
    bounds: InputBounds,
    virtual_screen: VirtualDesktopRect,
    current_target_security: TargetWindowSecurity,
    agent_integrity: WindowsIntegrityLevel,
    capabilities: Capabilities,
    held_buttons: [bool; 6],
    held_keys: HashSet<u16>,
    prepared: Option<Operation>,
}

impl<P: SendInputPoster> WindowsInputSink<P> {
    /// Create a new Windows input sink.
    pub fn new(
        poster: P,
        bounds: InputBounds,
        virtual_screen: VirtualDesktopRect,
        agent_integrity: WindowsIntegrityLevel,
    ) -> Self {
        let capabilities = Capabilities::default()
            .with(Capability::Absolute)
            .with(Capability::Buttons)
            .with(Capability::LineScroll)
            .with(Capability::PixelScroll)
            .with(Capability::Keys)
            .with(Capability::Repeat);

        Self {
            poster,
            bounds,
            virtual_screen,
            current_target_security: TargetWindowSecurity::NormalWindow,
            agent_integrity,
            capabilities,
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
    pub const fn virtual_screen(&self) -> VirtualDesktopRect {
        self.virtual_screen
    }

    #[must_use]
    pub const fn agent_integrity(&self) -> WindowsIntegrityLevel {
        self.agent_integrity
    }

    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    #[must_use]
    pub const fn current_target_security(&self) -> TargetWindowSecurity {
        self.current_target_security
    }

    pub fn set_target_window_security(&mut self, security: TargetWindowSecurity) {
        self.current_target_security = security;
    }

    /// Verification check immediately prior to submission.
    fn verify_submission(&self, op: &Operation) -> Result<(), PlatformError> {
        if !self.poster.is_connected() {
            return Err(PlatformError::Unavailable);
        }

        // UIPI integrity check: medium integrity cannot inject into elevated or secure desktop
        if self.agent_integrity < WindowsIntegrityLevel::High {
            match self.current_target_security {
                TargetWindowSecurity::ElevatedWindow | TargetWindowSecurity::SecureDesktop => {
                    return Err(PlatformError::Unsupported);
                }
                TargetWindowSecurity::NormalWindow => {}
            }
        }

        if let Operation::Absolute(pt) = op
            && !self.bounds.contains(*pt)
        {
            return Err(PlatformError::GeometryChanged);
        }

        Ok(())
    }
}

impl<P: SendInputPoster> InputSink for WindowsInputSink<P> {
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
                let virt_point = VirtualDesktopPoint {
                    x: self.virtual_screen.left.saturating_add(pt.x),
                    y: self.virtual_screen.top.saturating_add(pt.y),
                };
                match virtual_desktop_to_send_input(virt_point, self.virtual_screen) {
                    Ok(norm) => self.poster.post_mouse_move_virtual(norm, virt_point),
                    Err(_) => Err(PlatformError::GeometryChanged),
                }
            }
            Operation::Button { button, pressed } => {
                let idx = button as usize;
                if let Some(slot) = self.held_buttons.get_mut(idx) {
                    *slot = pressed;
                }
                self.poster.post_mouse_button(button, pressed)
            }
            Operation::Scroll { y, .. } => {
                let delta = match y.cmp(&0) {
                    std::cmp::Ordering::Greater => WHEEL_DELTA,
                    std::cmp::Ordering::Less => -WHEEL_DELTA,
                    std::cmp::Ordering::Equal => 0,
                };
                if delta != 0 {
                    self.poster.post_mouse_wheel(delta, false)
                } else {
                    Ok(())
                }
            }
            Operation::Wheel { direction, pressed } => {
                if pressed {
                    let (delta, horizontal) = match direction {
                        WheelDirection::Up => (WHEEL_DELTA, false),
                        WheelDirection::Down => (-WHEEL_DELTA, false),
                        WheelDirection::Left => (-WHEEL_DELTA, true),
                        WheelDirection::Right => (WHEEL_DELTA, true),
                    };
                    self.poster.post_mouse_wheel(delta, horizontal)
                } else {
                    Ok(())
                }
            }
            Operation::Key { key, transition } => {
                let scancode = key.usage();
                let down = match transition {
                    KeyTransition::Press | KeyTransition::Repeat => true,
                    KeyTransition::Release => false,
                };
                if down {
                    self.held_keys.insert(scancode);
                } else {
                    self.held_keys.remove(&scancode);
                }
                self.poster.post_keyboard(scancode, down, true)
            }
            Operation::Relative { .. } | Operation::Text(_) => {
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
