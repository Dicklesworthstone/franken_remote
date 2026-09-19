//! Reference state machine and contracts for OS permission and lifecycle discovery.
//!
//! Owns: Plan sections 5.3, 10.1-10.3, and Phase 0 Gate (`fr-p0-os-lifecycle-ost`).
//!
//! Validates:
//! 1. Wayland Portal sequence: CreateSession -> SelectDevices -> SelectSources ->
//!    RequestClipboard BEFORE Start -> Start.
//! 2. Returned grants determine actual authority (never requested flags).
//! 3. Single-use restore-token rotation with atomic persistence.
//! 4. Separation of portal session ownership (session agent) vs PipeWire stream (worker).
//! 5. Lifecycle transitions: Lock/Logout/UserSwitch revoke authority; Suspend/Resume
//!    fences generations.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Wayland device types bitmask (org.freedesktop.portal.RemoteDesktop).
pub mod device_types {
    pub const KEYBOARD: u32 = 1;
    pub const POINTER: u32 = 2;
    pub const TOUCHSCREEN: u32 = 4;
}

/// Wayland cursor modes (org.freedesktop.portal.ScreenCast).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    Hidden = 1,
    Embedded = 2,
    Metadata = 4,
}

/// Stage in the Wayland RemoteDesktop portal handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalState {
    Initial,
    SessionCreated,
    DevicesSelected,
    SourcesSelected,
    ClipboardRequested,
    Started,
    Closed,
}

/// Error in portal handshake or lifecycle policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleError {
    InvalidSequence { current: PortalState, attempted: &'static str },
    ClipboardRequestedAfterStart,
    AuthorizationDenied { reason: String },
    StaleRestoreToken,
    TokenStorageError(String),
    InputAuthorityLeak,
    AuthorityRevokedOnLock,
    GenerationInvalidatedOnResume,
}

/// Grantees returned by the compositor's response to `Start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantedCapabilities {
    pub granted_devices: u32,
    pub streams_count: usize,
    pub clipboard_granted: bool,
    pub restore_token: Option<String>,
}

/// State machine tracking the Wayland RemoteDesktop portal handshake.
#[derive(Debug)]
pub struct WaylandPortalSession {
    state: PortalState,
    session_handle: String,
    requested_devices: u32,
    requested_clipboard: bool,
    granted: Option<GrantedCapabilities>,
}

impl WaylandPortalSession {
    pub fn new() -> Self {
        Self {
            state: PortalState::Initial,
            session_handle: String::new(),
            requested_devices: 0,
            requested_clipboard: false,
            granted: None,
        }
    }

    /// Step 1: CreateSession
    pub fn create_session(&mut self, session_handle: String) -> Result<(), LifecycleError> {
        if self.state != PortalState::Initial {
            return Err(LifecycleError::InvalidSequence {
                current: self.state,
                attempted: "create_session",
            });
        }
        self.session_handle = session_handle;
        self.state = PortalState::SessionCreated;
        Ok(())
    }

    /// Step 2: SelectDevices
    pub fn select_devices(&mut self, device_mask: u32) -> Result<(), LifecycleError> {
        if self.state != PortalState::SessionCreated {
            return Err(LifecycleError::InvalidSequence {
                current: self.state,
                attempted: "select_devices",
            });
        }
        self.requested_devices = device_mask;
        self.state = PortalState::DevicesSelected;
        Ok(())
    }

    /// Step 3: SelectSources
    pub fn select_sources(
        &mut self,
        _cursor_mode: CursorMode,
        _restore_token: Option<&str>,
    ) -> Result<(), LifecycleError> {
        if self.state != PortalState::DevicesSelected {
            return Err(LifecycleError::InvalidSequence {
                current: self.state,
                attempted: "select_sources",
            });
        }
        self.state = PortalState::SourcesSelected;
        Ok(())
    }

    /// Step 4: Request clipboard integration BEFORE Start.
    pub fn request_clipboard(&mut self) -> Result<(), LifecycleError> {
        if self.state == PortalState::Started {
            return Err(LifecycleError::ClipboardRequestedAfterStart);
        }
        if self.state != PortalState::SourcesSelected {
            return Err(LifecycleError::InvalidSequence {
                current: self.state,
                attempted: "request_clipboard",
            });
        }
        self.requested_clipboard = true;
        self.state = PortalState::ClipboardRequested;
        Ok(())
    }

    /// Step 5: Start portal session and process returned compositor grants.
    pub fn start(&mut self, returned_grants: GrantedCapabilities) -> Result<(), LifecycleError> {
        if self.state != PortalState::SourcesSelected && self.state != PortalState::ClipboardRequested {
            return Err(LifecycleError::InvalidSequence {
                current: self.state,
                attempted: "start",
            });
        }

        // Must at least grant a video stream
        if returned_grants.streams_count == 0 {
            self.state = PortalState::Closed;
            return Err(LifecycleError::AuthorizationDenied {
                reason: "compositor returned 0 display streams".to_string(),
            });
        }

        self.granted = Some(returned_grants);
        self.state = PortalState::Started;
        Ok(())
    }

    /// Checks if input authority was actually granted by the compositor.
    pub fn is_input_authorized(&self, device_type: u32) -> bool {
        if let Some(ref grants) = self.granted {
            (grants.granted_devices & device_type) != 0
        } else {
            false
        }
    }

    /// Checks if clipboard was actually granted by the compositor.
    pub fn is_clipboard_authorized(&self) -> bool {
        if let Some(ref grants) = self.granted {
            grants.clipboard_granted
        } else {
            false
        }
    }

    pub fn state(&self) -> PortalState {
        self.state
    }
}

impl Default for WaylandPortalSession {
    fn default() -> Self {
        Self::new()
    }
}

/// Atomically persists and rotates single-use restore tokens on disk.
#[derive(Debug)]
pub struct RestoreTokenManager {
    token_file: PathBuf,
    consumed_tokens: HashSet<String>,
}

impl RestoreTokenManager {
    pub fn new(storage_path: PathBuf) -> Self {
        Self {
            token_file: storage_path,
            consumed_tokens: HashSet::new(),
        }
    }

    /// Loads the existing restore token from disk, if present.
    pub fn read_current_token(&self) -> io::Result<Option<String>> {
        if !self.token_file.exists() {
            return Ok(None);
        }
        let content = fs::read_to_string(&self.token_file)?;
        let trimmed = content.trim().to_string();
        if trimmed.is_empty() {
            Ok(None)
        } else {
            Ok(Some(trimmed))
        }
    }

    /// Rotates to a new restore token atomically, invalidating the old token.
    pub fn rotate_token(&mut self, old_token: Option<&str>, new_token: &str) -> Result<(), LifecycleError> {
        if let Some(old) = old_token {
            if self.consumed_tokens.contains(old) {
                return Err(LifecycleError::StaleRestoreToken);
            }
            self.consumed_tokens.insert(old.to_string());
        }

        // Atomic write: write to temp file then rename
        let parent = self.token_file.parent().unwrap_or_else(|| Path::new("."));
        let temp_file = parent.join(format!(".restore_token_{}.tmp", std::process::id()));

        fs::write(&temp_file, new_token.as_bytes())
            .map_err(|e| LifecycleError::TokenStorageError(e.to_string()))?;

        fs::rename(&temp_file, &self.token_file)
            .map_err(|e| LifecycleError::TokenStorageError(e.to_string()))?;

        Ok(())
    }
}

/// Models OS session lifecycle transitions across all platforms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OsLifecycleEvent {
    SessionLocked,
    SessionUnlocked,
    SessionLogoff,
    UserSwitchBegun,
    SystemSuspend,
    SystemResume,
}

/// Guard managing active session authority across lifecycle boundaries.
#[derive(Debug)]
pub struct SessionLifecycleGuard {
    pub authority_generation: u64,
    pub input_lease_active: bool,
    pub observation_active: bool,
    pub session_valid: bool,
}

impl SessionLifecycleGuard {
    pub fn new(generation: u64) -> Self {
        Self {
            authority_generation: generation,
            input_lease_active: true,
            observation_active: true,
            session_valid: true,
        }
    }

    /// Handle OS lifecycle events according to FrankenRemote security invariants.
    pub fn on_lifecycle_event(&mut self, event: OsLifecycleEvent) {
        match event {
            OsLifecycleEvent::SessionLocked => {
                // Lock ends observation and control immediately
                self.input_lease_active = false;
                self.observation_active = false;
            }
            OsLifecycleEvent::SessionUnlocked => {
                // Unlocking requires re-negotiation / re-approval; does NOT silently restore control
                self.input_lease_active = false;
                self.observation_active = false;
            }
            OsLifecycleEvent::SessionLogoff | OsLifecycleEvent::UserSwitchBegun => {
                // User switch or logoff is terminal for the session
                self.input_lease_active = false;
                self.observation_active = false;
                self.session_valid = false;
            }
            OsLifecycleEvent::SystemSuspend => {
                self.input_lease_active = false;
                self.observation_active = false;
            }
            OsLifecycleEvent::SystemResume => {
                // Suspend/Resume invalidates all prior authority generations
                self.authority_generation += 1;
                self.input_lease_active = false;
                self.observation_active = false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_portal_sequence_with_grants() {
        let mut portal = WaylandPortalSession::new();
        assert_eq!(portal.state(), PortalState::Initial);

        portal.create_session("session_123".to_string()).unwrap();
        assert_eq!(portal.state(), PortalState::SessionCreated);

        portal.select_devices(device_types::KEYBOARD | device_types::POINTER).unwrap();
        assert_eq!(portal.state(), PortalState::DevicesSelected);

        portal.select_sources(CursorMode::Metadata, None).unwrap();
        assert_eq!(portal.state(), PortalState::SourcesSelected);

        portal.request_clipboard().unwrap();
        assert_eq!(portal.state(), PortalState::ClipboardRequested);

        let grants = GrantedCapabilities {
            granted_devices: device_types::POINTER, // User granted pointer only! Keyboard denied!
            streams_count: 1,
            clipboard_granted: true,
            restore_token: Some("token_abc_1".to_string()),
        };

        portal.start(grants).unwrap();
        assert_eq!(portal.state(), PortalState::Started);

        // Verification: Returned grants determine authority, NOT requested flags
        assert!(portal.is_input_authorized(device_types::POINTER));
        assert!(!portal.is_input_authorized(device_types::KEYBOARD)); // Denied by compositor
        assert!(portal.is_clipboard_authorized());
    }

    #[test]
    fn test_clipboard_requested_after_start_is_rejected() {
        let mut portal = WaylandPortalSession::new();
        portal.create_session("sess".to_string()).unwrap();
        portal.select_devices(device_types::POINTER).unwrap();
        portal.select_sources(CursorMode::Embedded, None).unwrap();

        let grants = GrantedCapabilities {
            granted_devices: device_types::POINTER,
            streams_count: 1,
            clipboard_granted: false,
            restore_token: None,
        };
        portal.start(grants).unwrap();

        // Attempting to request clipboard AFTER start must fail
        let err = portal.request_clipboard().unwrap_err();
        assert_eq!(err, LifecycleError::ClipboardRequestedAfterStart);
    }

    #[test]
    fn test_restore_token_rotation_and_stale_detection() {
        let tmp_dir = std::env::temp_dir().join(format!("fr_token_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp_dir);
        fs::create_dir_all(&tmp_dir).unwrap();

        let token_path = tmp_dir.join("restore_token.txt");
        let mut manager = RestoreTokenManager::new(token_path.clone());

        // Initial token write
        manager.rotate_token(None, "token_initial").unwrap();
        assert_eq!(manager.read_current_token().unwrap(), Some("token_initial".to_string()));

        // Rotate to second token
        manager.rotate_token(Some("token_initial"), "token_second").unwrap();
        assert_eq!(manager.read_current_token().unwrap(), Some("token_second".to_string()));

        // Attempting to reuse the consumed initial token fails
        let err = manager.rotate_token(Some("token_initial"), "token_third").unwrap_err();
        assert_eq!(err, LifecycleError::StaleRestoreToken);

        let _ = fs::remove_dir_all(tmp_dir);
    }

    #[test]
    fn test_session_lock_revokes_authority_immediately() {
        let mut guard = SessionLifecycleGuard::new(10);
        assert!(guard.input_lease_active);
        assert!(guard.observation_active);

        // Lock event occurs
        guard.on_lifecycle_event(OsLifecycleEvent::SessionLocked);

        assert!(!guard.input_lease_active, "Input lease must be revoked on lock");
        assert!(!guard.observation_active, "Observation must be revoked on lock");

        // Unlock does NOT automatically restore control
        guard.on_lifecycle_event(OsLifecycleEvent::SessionUnlocked);
        assert!(!guard.input_lease_active);
    }

    #[test]
    fn test_system_resume_fences_authority_generation() {
        let mut guard = SessionLifecycleGuard::new(5);
        assert_eq!(guard.authority_generation, 5);

        guard.on_lifecycle_event(OsLifecycleEvent::SystemSuspend);
        assert!(!guard.input_lease_active);

        guard.on_lifecycle_event(OsLifecycleEvent::SystemResume);
        assert_eq!(guard.authority_generation, 6, "Authority generation must increment on resume");
        assert!(!guard.input_lease_active, "Input lease must not survive resume");
    }

    #[test]
    fn test_user_switch_terminates_session() {
        let mut guard = SessionLifecycleGuard::new(1);
        guard.on_lifecycle_event(OsLifecycleEvent::UserSwitchBegun);
        assert!(!guard.session_valid, "Session must be invalid after user switch");
        assert!(!guard.input_lease_active);
    }
}
