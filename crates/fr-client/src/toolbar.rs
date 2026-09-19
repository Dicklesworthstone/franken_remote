//! Minimal desktop client toolbar model (Plan §16.1).
//!
//! # Invariants:
//! - Provides the minimal UI components specified in Plan §16.1: machine/host
//!   identifier, display choice, connection-state indicator, revoke-visible state,
//!   shortcut-capture toggle, and diagnostics summary.
//! - Read-only view of session authority: observation and control remain distinct.
//! - Shortcut capture toggle reflects the underlying `ShortcutCaptureController` state.
//! - Local revoke state is always visible when observation or control is active (§15.2).

use crate::session::{ClientSession, SessionState};
use crate::shortcut::{ShortcutCaptureController, ShortcutCaptureError, ShortcutCaptureMode};

/// Minimal desktop toolbar state model for presentation in client window chrome.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolbarModel {
    /// Remote machine identifier or canonical tailnet FQDN.
    pub host_info: Option<String>,
    /// Handle of the currently observed display (if selected).
    pub selected_display: Option<u128>,
    /// High-level session lifecycle state label.
    pub session_state_label: &'static str,
    /// Whether connection to host is established (viewing or controlling).
    pub is_connected: bool,
    /// Whether input control is currently held.
    pub is_controlling: bool,
    /// Local revoke indicator visibility: true when session is active and revocable.
    pub revoke_visible: bool,
    /// Current shortcut capture routing mode.
    pub shortcut_mode: ShortcutCaptureMode,
    /// Whether shortcut capture is supported on the current client platform.
    pub shortcut_supported: bool,
    /// Status description of the shortcut capture state.
    pub shortcut_status_text: &'static str,
    /// Estimated round-trip latency in milliseconds, if available.
    pub latency_ms: Option<u32>,
}

impl Default for ToolbarModel {
    fn default() -> Self {
        Self {
            host_info: None,
            selected_display: None,
            session_state_label: "Disconnected",
            is_connected: false,
            is_controlling: false,
            revoke_visible: false,
            shortcut_mode: ShortcutCaptureMode::Disabled,
            shortcut_supported: true,
            shortcut_status_text: "Local System",
            latency_ms: None,
        }
    }
}

impl ToolbarModel {
    /// Create a new toolbar in default disconnected state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set remote host information.
    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host_info = Some(host.into());
        self
    }

    /// Set selected display handle.
    #[must_use]
    pub const fn with_display(mut self, display: u128) -> Self {
        self.selected_display = Some(display);
        self
    }

    /// Synchronize toolbar display with current `ClientSession` state.
    pub fn update_from_session(&mut self, session: &ClientSession) {
        self.session_state_label = session.state().display_label();

        match session.state() {
            SessionState::Disconnected
            | SessionState::Closed { .. }
            | SessionState::Connecting { .. }
            | SessionState::Reconnecting { .. } => {
                self.is_connected = false;
                self.is_controlling = false;
                self.revoke_visible = false;
            }
            SessionState::WaitingApproval { .. } => {
                self.is_connected = false;
                self.is_controlling = false;
                self.revoke_visible = true; // User can revoke / cancel pending approval
            }
            SessionState::Viewing { .. }
            | SessionState::RequestingControl { .. }
            | SessionState::Suspended { .. } => {
                self.is_connected = true;
                self.is_controlling = false;
                self.revoke_visible = true; // Observation is revocable locally
            }
            SessionState::Controlling { input_ready, .. } => {
                self.is_connected = true;
                self.is_controlling = *input_ready;
                self.revoke_visible = true;
            }
        }
    }

    /// Synchronize toolbar display with `ShortcutCaptureController`.
    pub fn update_from_shortcuts(&mut self, controller: &ShortcutCaptureController) {
        self.shortcut_mode = controller.mode();
        self.shortcut_supported = controller.can_enable().is_ok() || controller.is_enabled();

        self.shortcut_status_text = match controller.mode() {
            ShortcutCaptureMode::Enabled => "Routing to Remote",
            ShortcutCaptureMode::Disabled => {
                if self.shortcut_supported {
                    "Local System"
                } else {
                    "Unsupported"
                }
            }
        };
    }

    /// Toggle shortcut capture via the provided controller and update toolbar display.
    pub fn toggle_shortcut_capture(
        &mut self,
        controller: &mut ShortcutCaptureController,
    ) -> Result<ShortcutCaptureMode, ShortcutCaptureError> {
        let new_mode = controller.toggle()?;
        self.update_from_shortcuts(controller);
        Ok(new_mode)
    }

    /// Format a single-line text summary of the toolbar for CLI output or TUI status bars.
    #[must_use]
    pub fn status_line(&self) -> String {
        let host = self.host_info.as_deref().unwrap_or("None");
        let display = self
            .selected_display
            .map_or_else(|| "Default".to_string(), |d| format!("{d}"));
        let revoke = if self.revoke_visible {
            "[Revoke: Ready]"
        } else {
            "[Revoke: Off]"
        };
        let shortcuts = format!("[Shortcuts: {}]", self.shortcut_status_text);

        format!(
            "Host: {host} | Disp: {display} | State: {} | {revoke} | {shortcuts}",
            self.session_state_label
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ReconnectPolicy;
    use crate::shortcut::PlatformCapabilityRow;

    #[test]
    fn toolbar_reflects_session_lifecycle_and_revoke_visibility() {
        let mut session = ClientSession::new(ReconnectPolicy::default());
        let mut toolbar = ToolbarModel::new().with_host("desktop.tailnet.net");

        // Initial state
        toolbar.update_from_session(&session);
        assert_eq!(toolbar.session_state_label, "Disconnected");
        assert!(!toolbar.is_connected);
        assert!(!toolbar.is_controlling);
        assert!(!toolbar.revoke_visible);

        // Connecting
        let now = crate::input::ClientInstant(1000);
        session.connect(now).unwrap();
        toolbar.update_from_session(&session);
        assert_eq!(toolbar.session_state_label, "Connecting");
        assert!(!toolbar.revoke_visible);

        // Waiting approval
        let req = fr_core::ids::RemoteSessionId::from_raw(1);
        session.on_waiting_approval(req, 5000).unwrap();
        toolbar.update_from_session(&session);
        assert_eq!(toolbar.session_state_label, "Waiting for Approval");
        assert!(toolbar.revoke_visible);

        // Viewing
        let binding = fr_wire::negotiation::ControlBinding {
            id: 1,
            host_boot: fr_core::ids::HostBootId::from_raw(1),
            os_session: fr_core::ids::OsSessionId::from_raw(1),
            remote_session: req,
        };
        session.on_session_opened(binding, now).unwrap();
        toolbar.update_from_session(&session);
        assert_eq!(toolbar.session_state_label, "Viewing (Stale Source)");
        assert!(toolbar.is_connected);
        assert!(!toolbar.is_controlling);
        assert!(toolbar.revoke_visible);

        // Freshness proven
        session.update_view_freshness(true, now).unwrap();
        toolbar.update_from_session(&session);
        assert_eq!(toolbar.session_state_label, "Viewing (Fresh)");

        // Control granted
        session.request_control(now).unwrap();
        let lease = fr_core::ids::InputLeaseId::from_raw(10);
        let ticket = fr_core::ids::InputTicketId::from_raw(20);
        session.on_control_granted(lease, ticket).unwrap();
        toolbar.update_from_session(&session);
        assert_eq!(toolbar.session_state_label, "Controlling");
        assert!(toolbar.is_controlling);
        assert!(toolbar.revoke_visible);

        let status = toolbar.status_line();
        assert!(status.contains("Host: desktop.tailnet.net"));
        assert!(status.contains("State: Controlling"));
        assert!(status.contains("[Revoke: Ready]"));
    }

    #[test]
    fn toolbar_toggles_shortcut_mode_and_updates_status() {
        let row = PlatformCapabilityRow::for_platform(crate::shortcut::PlatformId::LinuxX11);
        let mut controller = ShortcutCaptureController::new(row);
        let mut toolbar = ToolbarModel::new();

        toolbar.update_from_shortcuts(&controller);
        assert_eq!(toolbar.shortcut_status_text, "Local System");
        assert_eq!(toolbar.shortcut_mode, ShortcutCaptureMode::Disabled);

        let new_mode = toolbar
            .toggle_shortcut_capture(&mut controller)
            .expect("must toggle");
        assert_eq!(new_mode, ShortcutCaptureMode::Enabled);
        assert_eq!(toolbar.shortcut_status_text, "Routing to Remote");
        assert_eq!(toolbar.shortcut_mode, ShortcutCaptureMode::Enabled);
    }
}
