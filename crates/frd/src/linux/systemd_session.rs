//! Systemd user service graphical session and D-Bus attachment (plan §10.1).
//!
//! A systemd user service must attach to the correct graphical session and its D-Bus
//! environment rather than assuming static or system-service environment variables identify
//! the desktop.
//!
//! Enforces:
//! 1. Detection of session type: Wayland vs X11 from `$XDG_SESSION_TYPE`.
//! 2. Validation of display environment: `$WAYLAND_DISPLAY` for Wayland, `$DISPLAY` for X11.
//! 3. Validation of D-Bus session bus: `$DBUS_SESSION_BUS_ADDRESS` or standard fallback `/run/user/<uid>/bus`.
//! 4. Desktop environment detection: `$XDG_CURRENT_DESKTOP` (GNOME, KDE, Hyprland, etc.).

use core::fmt;
use std::env;
use std::path::PathBuf;

/// Operating display server type detected from graphical environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionType {
    Wayland,
    X11,
    Unknown,
}

impl SessionType {
    #[must_use]
    pub fn from_env_value(val: &str) -> Self {
        if val.eq_ignore_ascii_case("wayland") {
            Self::Wayland
        } else if val.eq_ignore_ascii_case("x11") {
            Self::X11
        } else {
            Self::Unknown
        }
    }
}

/// Resolved graphical session environment variables for desktop hosting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphicalSessionEnvironment {
    pub session_type: SessionType,
    pub wayland_display: Option<String>,
    pub x11_display: Option<String>,
    pub dbus_session_bus_address: Option<String>,
    pub current_desktop: Option<String>,
    pub user_runtime_dir: Option<PathBuf>,
}

impl GraphicalSessionEnvironment {
    /// Detect current graphical session environment variables.
    #[must_use]
    pub fn detect() -> Self {
        let session_type = env::var("XDG_SESSION_TYPE").map_or_else(
            |_| {
                if env::var("WAYLAND_DISPLAY").is_ok() {
                    SessionType::Wayland
                } else if env::var("DISPLAY").is_ok() {
                    SessionType::X11
                } else {
                    SessionType::Unknown
                }
            },
            |s| SessionType::from_env_value(&s),
        );

        let wayland_display = env::var("WAYLAND_DISPLAY").ok();
        let x11_display = env::var("DISPLAY").ok();
        let dbus_session_bus_address = env::var("DBUS_SESSION_BUS_ADDRESS").ok();
        let current_desktop = env::var("XDG_CURRENT_DESKTOP").ok();
        let user_runtime_dir = env::var("XDG_RUNTIME_DIR").ok().map(PathBuf::from);

        Self {
            session_type,
            wayland_display,
            x11_display,
            dbus_session_bus_address,
            current_desktop,
            user_runtime_dir,
        }
    }

    /// Validate whether the detected environment is viable for remote desktop hosting.
    pub fn validate_for_hosting(&self) -> Result<(), SessionEnvironmentError> {
        // Check D-Bus session bus availability
        if self.dbus_session_bus_address.is_none() {
            // Check fallback to /run/user/<uid>/bus
            let has_standard_bus = self
                .user_runtime_dir
                .as_ref()
                .is_some_and(|p| p.join("bus").exists());

            if !has_standard_bus {
                return Err(SessionEnvironmentError::DbusSessionBusMissing);
            }
        }

        match self.session_type {
            SessionType::Wayland => {
                if self.wayland_display.is_none() {
                    return Err(SessionEnvironmentError::WaylandDisplayMissing);
                }
            }
            SessionType::X11 => {
                if self.x11_display.is_none() {
                    return Err(SessionEnvironmentError::X11DisplayMissing);
                }
            }
            SessionType::Unknown => {
                if self.wayland_display.is_none() && self.x11_display.is_none() {
                    return Err(SessionEnvironmentError::NoDisplayDetected);
                }
            }
        }

        Ok(())
    }
}

/// Typed errors when inspecting or attaching to the graphical session environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnvironmentError {
    WaylandDisplayMissing,
    X11DisplayMissing,
    NoDisplayDetected,
    DbusSessionBusMissing,
}

impl fmt::Display for SessionEnvironmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WaylandDisplayMissing => write!(
                f,
                "Wayland session detected but $WAYLAND_DISPLAY is not set"
            ),
            Self::X11DisplayMissing => write!(f, "X11 session detected but $DISPLAY is not set"),
            Self::NoDisplayDetected => write!(
                f,
                "no active graphical display found (neither $WAYLAND_DISPLAY nor $DISPLAY set)"
            ),
            Self::DbusSessionBusMissing => write!(
                f,
                "D-Bus session bus unavailable; service cannot reach desktop portals"
            ),
        }
    }
}

impl std::error::Error for SessionEnvironmentError {}
