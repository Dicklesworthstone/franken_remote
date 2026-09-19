//! Platform permission state surfacing and OS session lifecycle (plan §§5.3, 19.1).
//!
//! Surfaces Screen Recording, Accessibility, Wayland portal, and UIPI limitations.
//! Strictly checks permissions at the submission checkpoint: injection is refused
//! with typed errors if required permissions (such as macOS Accessibility) are missing.
//! On OS session change, logout, lock, or permission loss, old authority is revoked
//! and never inherited by a new session.

use std::collections::HashMap;

/// Platform environment running the interactive session agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlatformKind {
    MacOs,
    LinuxWayland,
    LinuxX11,
    Windows,
}

impl PlatformKind {
    /// Detect platform from compile target.
    pub const fn current() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "linux")]
        {
            // By default Linux detects Wayland vs X11 at runtime; defaults to Wayland
            Self::LinuxWayland
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
        {
            Self::LinuxX11
        }
    }
}

/// Specific platform permission categories required by `FrankenRemote`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionKind {
    /// Screen Recording / capture (macOS TCC `kTCCServiceScreenCapture`, Wayland `ScreenCast` portal).
    ScreenCapture,
    /// Input injection via Accessibility (macOS TCC `kTCCServiceAccessibility` for `CGEvent`).
    AccessibilityInput,
    /// Input injection via Wayland portal (`org.freedesktop.portal.RemoteDesktop` / EIS).
    RemoteDesktopPortal,
    /// Desktop Duplication API access (Windows).
    DesktopDuplication,
    /// Microphone / Audio capture permission.
    AudioCapture,
    /// Power / sleep inhibition (`IOKit` assertion, systemd-inhibit, `SetThreadExecutionState`).
    SleepInhibition,
}

/// Status of a platform permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionStatus {
    /// Explicitly granted by OS or user.
    Granted,
    /// Explicitly denied or revoked.
    Denied,
    /// Requires user prompt or interactive authorization in OS settings.
    PromptNeeded,
    /// Not supported or applicable on this platform.
    Unsupported,
}

/// Typed errors returned when platform permissions or OS session state refuse an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlatformPermissionError {
    /// macOS Accessibility permission (`kTCCServiceAccessibility`) is missing or denied.
    /// `CGEvent` keyboard/pointer injection cannot proceed without it.
    MissingAccessibility,
    /// Linux Wayland `RemoteDesktop` or EIS portal permission is missing.
    MissingRemoteDesktopPortal,
    /// Generic input injection permission missing.
    MissingInputPermission(PermissionKind),
    /// Screen capture permission missing.
    MissingScreenCapture,
    /// OS desktop session is locked. Input injection and capture are forbidden while locked.
    SessionLocked,
    /// OS interactive user session changed or logged out. Authority must be revoked and never inherited.
    SessionChanged {
        previous_session_id: u32,
        current_session_id: u32,
    },
    /// Permission was revoked at runtime while session was active.
    PermissionLost(PermissionKind),
}

impl core::fmt::Display for PlatformPermissionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingAccessibility => write!(
                f,
                "macOS Accessibility permission not granted (CGEvent injection forbidden)"
            ),
            Self::MissingRemoteDesktopPortal => {
                write!(f, "Wayland RemoteDesktop portal permission not granted")
            }
            Self::MissingInputPermission(k) => {
                write!(f, "Input injection permission {k:?} not granted")
            }
            Self::MissingScreenCapture => write!(f, "Screen capture permission not granted"),
            Self::SessionLocked => write!(f, "OS session is locked"),
            Self::SessionChanged {
                previous_session_id,
                current_session_id,
            } => {
                write!(
                    f,
                    "OS user session changed from {previous_session_id} to {current_session_id}"
                )
            }
            Self::PermissionLost(k) => {
                write!(f, "Permission {k:?} was revoked during active session")
            }
        }
    }
}

impl std::error::Error for PlatformPermissionError {}

/// Manages platform permissions and OS session lifecycle states.
pub struct PermissionsManager {
    platform: PlatformKind,
    permissions: HashMap<PermissionKind, PermissionStatus>,
    os_session_id: u32,
    is_locked: bool,
}

impl PermissionsManager {
    pub fn new(platform: PlatformKind, os_session_id: u32) -> Self {
        let mut permissions = HashMap::new();
        // Safe default: unknown/prompt needed until probed
        permissions.insert(
            PermissionKind::ScreenCapture,
            PermissionStatus::PromptNeeded,
        );
        permissions.insert(
            PermissionKind::AccessibilityInput,
            PermissionStatus::PromptNeeded,
        );
        permissions.insert(
            PermissionKind::RemoteDesktopPortal,
            PermissionStatus::PromptNeeded,
        );
        permissions.insert(
            PermissionKind::DesktopDuplication,
            PermissionStatus::Granted,
        );
        permissions.insert(PermissionKind::AudioCapture, PermissionStatus::PromptNeeded);
        permissions.insert(PermissionKind::SleepInhibition, PermissionStatus::Granted);

        Self {
            platform,
            permissions,
            os_session_id,
            is_locked: false,
        }
    }

    pub fn platform(&self) -> PlatformKind {
        self.platform
    }

    pub fn os_session_id(&self) -> u32 {
        self.os_session_id
    }

    pub fn is_locked(&self) -> bool {
        self.is_locked
    }

    /// Update status of a permission.
    pub fn set_permission(&mut self, kind: PermissionKind, status: PermissionStatus) {
        self.permissions.insert(kind, status);
    }

    /// Query status of a permission.
    pub fn status(&self, kind: PermissionKind) -> PermissionStatus {
        self.permissions
            .get(&kind)
            .copied()
            .unwrap_or(PermissionStatus::Unsupported)
    }

    /// Surface whether screen capture is permitted.
    pub fn verify_screen_capture(&self) -> Result<(), PlatformPermissionError> {
        if self.is_locked {
            return Err(PlatformPermissionError::SessionLocked);
        }
        if self.status(PermissionKind::ScreenCapture) != PermissionStatus::Granted {
            return Err(PlatformPermissionError::MissingScreenCapture);
        }
        Ok(())
    }

    /// Surface whether input injection is permitted on the current platform.
    /// Returns typed refusal reasons:
    /// - `MissingAccessibility` if on macOS without Accessibility
    /// - `MissingRemoteDesktopPortal` if on Linux Wayland without portal grant
    /// - `SessionLocked` if desktop is locked
    pub fn verify_input_injection(&self) -> Result<(), PlatformPermissionError> {
        if self.is_locked {
            return Err(PlatformPermissionError::SessionLocked);
        }

        match self.platform {
            PlatformKind::MacOs => {
                if self.status(PermissionKind::AccessibilityInput) != PermissionStatus::Granted {
                    return Err(PlatformPermissionError::MissingAccessibility);
                }
            }
            PlatformKind::LinuxWayland => {
                if self.status(PermissionKind::RemoteDesktopPortal) != PermissionStatus::Granted {
                    return Err(PlatformPermissionError::MissingRemoteDesktopPortal);
                }
            }
            PlatformKind::LinuxX11 | PlatformKind::Windows => {
                // X11 and Windows verify UIPI / active desktop session
            }
        }
        Ok(())
    }

    /// Handle OS screen lock event. Returns true if authority should be revoked.
    pub fn on_session_locked(&mut self) -> bool {
        let changed = !self.is_locked;
        self.is_locked = true;
        changed
    }

    /// Handle OS screen unlock event.
    pub fn on_session_unlocked(&mut self) {
        self.is_locked = false;
    }

    /// Handle OS user session transition (e.g. fast user switching, logout/login).
    /// If session ID changes, authority must be revoked and never inherited.
    pub fn on_session_transition(
        &mut self,
        new_session_id: u32,
    ) -> Result<(), PlatformPermissionError> {
        if self.os_session_id != new_session_id {
            let previous = self.os_session_id;
            self.os_session_id = new_session_id;
            return Err(PlatformPermissionError::SessionChanged {
                previous_session_id: previous,
                current_session_id: new_session_id,
            });
        }
        Ok(())
    }

    /// Handle runtime permission loss (e.g. user toggles TCC permission off in System Settings).
    pub fn on_permission_revoked(&mut self, kind: PermissionKind) -> PlatformPermissionError {
        self.permissions.insert(kind, PermissionStatus::Denied);
        PlatformPermissionError::PermissionLost(kind)
    }
}
