//! Explicit X11 host adapter and security diagnostics (plan §10.1).
//!
//! Unlike Wayland portals which enforce user consent prompts, per-client isolation,
//! and scoped capability grants, traditional X11 operates under a shared unconfined trust model:
//! 1. No window-level isolation: any connected client can read pixels from any window.
//! 2. Global input snooping: any client can monitor keystrokes and pointer events globally.
//! 3. Unconfined input injection: the `XTest` extension allows synthetic event injection
//!    without user consent prompts.
//!
//! This adapter surfaces these distinct security assumptions explicitly in diagnostics.
//! It implements `InputSink` with X11-specific requirements:
//! - `repeat_requires_pair = true` (X11 has no standalone repeat event).
//! - `line_scroll_requires_pairs = true` (discrete wheel events use button press/release pairs).

use fr_core::{
    input::{InputBounds, KeyTransition, PointerButton},
    input_submission::{
        Capabilities, Capability, InputSink, Operation, PlatformError, Submission,
        scroll::WheelDirection,
    },
};

/// Platform security model surfaced in diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformSecurityModel {
    /// Wayland with portal consent, per-client isolation, and scoped capabilities.
    WaylandPortalConfined,
    /// Traditional X11 with shared unconfined trust, no portal consent, and global snooping risk.
    X11Unconfined,
}

/// Diagnostic report detailing the security posture and limitations of the active display server.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct PlatformSecurityReport {
    pub model: PlatformSecurityModel,
    pub window_isolation: bool,
    pub consent_prompt_enforced: bool,
    pub input_snooping_protection: bool,
    pub synthetic_injection_confined: bool,
    pub diagnostic_warning: Option<&'static str>,
}

impl PlatformSecurityReport {
    /// Security report for Wayland portal environments.
    #[must_use]
    pub const fn wayland_portal() -> Self {
        Self {
            model: PlatformSecurityModel::WaylandPortalConfined,
            window_isolation: true,
            consent_prompt_enforced: true,
            input_snooping_protection: true,
            synthetic_injection_confined: true,
            diagnostic_warning: None,
        }
    }

    /// Security report for traditional X11 environments.
    #[must_use]
    pub const fn x11_unconfined() -> Self {
        Self {
            model: PlatformSecurityModel::X11Unconfined,
            window_isolation: false,
            consent_prompt_enforced: false,
            input_snooping_protection: false,
            synthetic_injection_confined: false,
            diagnostic_warning: Some(
                "X11 lacks per-client window and input isolation; any connected client can observe display contents and snoop keyboard/pointer events",
            ),
        }
    }
}

/// Poster for X11 events, to be implemented over libXtst.
///
/// frd has no production implementation: `frd run` injects through the
/// fr-native worker's own `XTest` path, not this sink. The only implementation
/// is the recording poster owned by `tests/linux_host_adapter_test.rs`, so no
/// build of this library carries a sink that records events and reports them
/// submitted.
pub trait X11EventPoster: Send + Sync {
    fn fake_motion_event(&mut self, x: i32, y: i32) -> Result<(), PlatformError>;
    fn fake_button_event(&mut self, button: u32, down: bool) -> Result<(), PlatformError>;
    fn fake_key_event(&mut self, keycode: u32, down: bool) -> Result<(), PlatformError>;
    fn is_connected(&self) -> bool;
}

/// X11 input sink implementing `InputSink` with X11 specific requirements.
pub struct X11InputSink<P: X11EventPoster> {
    poster: P,
    bounds: InputBounds,
    capabilities: Capabilities,
    prepared: Option<Operation>,
}

impl<P: X11EventPoster> X11InputSink<P> {
    pub fn new(poster: P, bounds: InputBounds) -> Self {
        let capabilities = Capabilities::default()
            .with(Capability::Absolute)
            .with(Capability::Buttons)
            .with(Capability::Keys)
            .with(Capability::LineScroll);

        Self {
            poster,
            bounds,
            capabilities,
            prepared: None,
        }
    }

    #[must_use]
    pub const fn bounds(&self) -> InputBounds {
        self.bounds
    }

    #[must_use]
    pub const fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    /// Return explicit security report reflecting the unconfined X11 trust model.
    #[must_use]
    pub const fn security_report(&self) -> PlatformSecurityReport {
        PlatformSecurityReport::x11_unconfined()
    }
}

impl<P: X11EventPoster> InputSink for X11InputSink<P> {
    fn prepare(&mut self, operation: Operation) -> Result<(), PlatformError> {
        if !self.poster.is_connected() {
            return Err(PlatformError::Unavailable);
        }

        if let Operation::Absolute(pt) = operation
            && !self.bounds.contains(pt)
        {
            return Err(PlatformError::GeometryChanged);
        }
        self.prepared = Some(operation);
        Ok(())
    }

    fn submit(&mut self, operation: Operation) -> Submission {
        if !self.poster.is_connected() {
            self.prepared = None;
            return Submission::NotSubmitted(PlatformError::Unavailable);
        }

        let res = match operation {
            Operation::Absolute(pt) => {
                if !self.bounds.contains(pt) {
                    return Submission::NotSubmitted(PlatformError::GeometryChanged);
                }
                self.poster.fake_motion_event(pt.x, pt.y)
            }
            Operation::Button { button, pressed } => {
                let btn_num = match button {
                    PointerButton::Primary => 1,
                    PointerButton::Secondary => 3,
                    PointerButton::Middle => 2,
                    PointerButton::Back => 8,
                    PointerButton::Forward => 9,
                };
                self.poster.fake_button_event(btn_num, pressed)
            }
            Operation::Key { key, transition } => {
                let keycode = u32::from(key.usage());
                let down = match transition {
                    KeyTransition::Press | KeyTransition::Repeat => true,
                    KeyTransition::Release => false,
                };
                self.poster.fake_key_event(keycode, down)
            }
            Operation::Scroll { y, .. } => {
                let btn = if y < 0 { 4 } else { 5 }; // 4 = scroll up, 5 = scroll down
                self.poster.fake_button_event(btn, true)
            }
            Operation::Wheel { direction, pressed } => {
                let btn = match direction {
                    WheelDirection::Up => 4,
                    WheelDirection::Down => 5,
                    WheelDirection::Left => 6,
                    WheelDirection::Right => 7,
                };
                self.poster.fake_button_event(btn, pressed)
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

    /// X11 has no standalone repeat event: require two separately authorized operations.
    fn repeat_requires_pair(&self) -> bool {
        true
    }

    /// Discrete wheel backends in X11 use button press/release pairs.
    fn line_scroll_requires_pairs(&self) -> bool {
        true
    }
}
