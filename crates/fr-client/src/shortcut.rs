//! Per-platform system shortcut capture and routing capability table (Plan §§15.1, 16.1).
//!
//! # Invariants:
//! - Browser/OS-reserved shortcuts, keyboard lock, IME composition, and native
//!   multitouch are advertised separately per platform (Plan §15.1).
//! - Shortcut capture is an explicit toggleable mode that routes system-reserved
//!   shortcuts to the remote desktop where the client OS permits it.
//! - Where the client OS forbids shortcut capture (e.g. mobile system gestures,
//!   or missing OS permissions), enabling is refused with a typed error.
//! - Toggle state transitions are logged with structured `[StateTrace: ...]` records.
//! - The toggle state is visible to minimal client toolbars and user chrome.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Supported target platforms for the `FrankenRemote` client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PlatformId {
    /// Linux X11 desktop environment.
    LinuxX11,
    /// Linux Wayland compositor environment.
    LinuxWayland,
    /// Windows desktop (Win32).
    Windows,
    /// macOS desktop (`AppKit`).
    MacOS,
    /// Standards-compliant web browser (WebCodecs/WASM).
    Browser,
    /// Android mobile client.
    Android,
    /// iOS mobile client.
    Ios,
}

impl PlatformId {
    /// Detect the current client platform from compilation target and runtime environment.
    #[must_use]
    pub fn current() -> Self {
        #[cfg(target_os = "linux")]
        {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                Self::LinuxWayland
            } else {
                Self::LinuxX11
            }
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(target_os = "macos")]
        {
            Self::MacOS
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self::Browser
        }
        #[cfg(target_os = "android")]
        {
            Self::Android
        }
        #[cfg(target_os = "ios")]
        {
            Self::Ios
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "windows",
            target_os = "macos",
            target_arch = "wasm32",
            target_os = "android",
            target_os = "ios"
        )))]
        {
            Self::LinuxX11
        }
    }

    /// Human-readable platform name.
    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::LinuxX11 => "Linux (X11)",
            Self::LinuxWayland => "Linux (Wayland)",
            Self::Windows => "Windows",
            Self::MacOS => "macOS",
            Self::Browser => "Web Browser",
            Self::Android => "Android",
            Self::Ios => "iOS",
        }
    }
}

/// OS-specific mechanism used to intercept and route system-reserved shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutRoutingMechanism {
    /// X11 active keyboard grab (`XGrabKeyboard`).
    X11KeyboardGrab,
    /// Wayland keyboard shortcuts inhibit protocol (`zwp_keyboard_shortcuts_inhibit_v1`).
    WaylandShortcutsInhibit,
    /// Windows low-level keyboard hook (`WH_KEYBOARD_LL`) / `RegisterHotKey`.
    Win32LowLevelHook,
    /// macOS CoreGraphics event tap (`CGEventTap`) with Accessibility privileges.
    MacOsEventTap,
    /// Web Keyboard Lock API (`navigator.keyboard.lock()`).
    BrowserKeyboardLock,
    /// No mechanism available on this platform.
    None,
}

impl ShortcutRoutingMechanism {
    /// Human-readable mechanism description.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::X11KeyboardGrab => "X11 active keyboard grab (XGrabKeyboard)",
            Self::WaylandShortcutsInhibit => "Wayland zwp_keyboard_shortcuts_inhibit_v1 protocol",
            Self::Win32LowLevelHook => "Win32 WH_KEYBOARD_LL low-level hook",
            Self::MacOsEventTap => "macOS CGEventTap with Accessibility trust",
            Self::BrowserKeyboardLock => "Web Keyboard Lock API (navigator.keyboard.lock)",
            Self::None => "None (system shortcuts reserved by OS)",
        }
    }
}

/// Degree of platform support for routing system-reserved shortcuts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutSupportStatus {
    /// Fully supported without extra user-granted system permissions.
    Supported,
    /// Supported, but requires specific OS/browser permission grant.
    SupportedWithPermission { permission_name: &'static str },
    /// Unsupported on this platform.
    Unsupported { reason: &'static str },
}

/// A capability row advertising per-platform shortcut routing per Plan §15.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformCapabilityRow {
    pub platform: PlatformId,
    pub mechanism: ShortcutRoutingMechanism,
    pub status: ShortcutSupportStatus,
    pub reserved_shortcuts_intercepted: &'static [&'static str],
    pub notes: &'static str,
}

impl PlatformCapabilityRow {
    /// Return the normative capability matrix across all defined platforms.
    #[must_use]
    pub const fn all_platforms() -> &'static [Self] {
        &[
            Self {
                platform: PlatformId::LinuxX11,
                mechanism: ShortcutRoutingMechanism::X11KeyboardGrab,
                status: ShortcutSupportStatus::Supported,
                reserved_shortcuts_intercepted: &["Alt+Tab", "Super", "Alt+F4", "Ctrl+Alt+Arrow"],
                notes: "Active keyboard grab while viewer window has focus.",
            },
            Self {
                platform: PlatformId::LinuxWayland,
                mechanism: ShortcutRoutingMechanism::WaylandShortcutsInhibit,
                status: ShortcutSupportStatus::SupportedWithPermission {
                    permission_name: "Compositor zwp_keyboard_shortcuts_inhibit_v1 support",
                },
                reserved_shortcuts_intercepted: &["Super", "Alt+Tab", "Ctrl+Alt+T"],
                notes: "Requires compositor implementation of keyboard shortcuts inhibit protocol.",
            },
            Self {
                platform: PlatformId::Windows,
                mechanism: ShortcutRoutingMechanism::Win32LowLevelHook,
                status: ShortcutSupportStatus::Supported,
                reserved_shortcuts_intercepted: &["Alt+Tab", "WinKey", "Alt+Esc"],
                notes: "Ctrl+Alt+Del is secure SAS and remains reserved by Windows kernel.",
            },
            Self {
                platform: PlatformId::MacOS,
                mechanism: ShortcutRoutingMechanism::MacOsEventTap,
                status: ShortcutSupportStatus::SupportedWithPermission {
                    permission_name: "Accessibility permissions in System Settings",
                },
                reserved_shortcuts_intercepted: &["Cmd+Tab", "Cmd+Space", "Mission Control"],
                notes: "Requires AXIsProcessTrusted trust in macOS System Settings.",
            },
            Self {
                platform: PlatformId::Browser,
                mechanism: ShortcutRoutingMechanism::BrowserKeyboardLock,
                status: ShortcutSupportStatus::SupportedWithPermission {
                    permission_name: "Fullscreen activation and user gesture",
                },
                reserved_shortcuts_intercepted: &["Escape (hold)", "Ctrl+W", "Alt+Tab (PWA)"],
                notes: "Keyboard Lock API requires secure HTTPS context and active fullscreen.",
            },
            Self {
                platform: PlatformId::Android,
                mechanism: ShortcutRoutingMechanism::None,
                status: ShortcutSupportStatus::Unsupported {
                    reason: "Android system navigation gestures and Home bar cannot be overridden",
                },
                reserved_shortcuts_intercepted: &[],
                notes: "Back button and hardware modifier keys routed while focused.",
            },
            Self {
                platform: PlatformId::Ios,
                mechanism: ShortcutRoutingMechanism::None,
                status: ShortcutSupportStatus::Unsupported {
                    reason: "iOS system gestures and Home indicator are strictly reserved",
                },
                reserved_shortcuts_intercepted: &[],
                notes: "Hardware keyboard modifiers pass through within window bounds.",
            },
        ]
    }

    /// Lookup capability row for a specific platform.
    #[must_use]
    pub fn for_platform(platform: PlatformId) -> Self {
        for row in Self::all_platforms() {
            if row.platform == platform {
                return row.clone();
            }
        }
        // Fallback default for undefined targets
        Self {
            platform,
            mechanism: ShortcutRoutingMechanism::None,
            status: ShortcutSupportStatus::Unsupported {
                reason: "Platform not qualified for shortcut capture",
            },
            reserved_shortcuts_intercepted: &[],
            notes: "No shortcut capture available.",
        }
    }
}

/// Shortcut capture mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ShortcutCaptureMode {
    /// System-reserved shortcuts are handled locally by the client OS.
    #[default]
    Disabled,
    /// System-reserved shortcuts are routed to the remote desktop session.
    Enabled,
}

impl ShortcutCaptureMode {
    #[must_use]
    pub const fn is_enabled(self) -> bool {
        matches!(self, Self::Enabled)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Disabled => "Local System",
            Self::Enabled => "Route to Remote",
        }
    }
}

/// Errors when attempting to toggle shortcut capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutCaptureError {
    /// Shortcut capture is not supported on this platform.
    UnsupportedPlatform {
        platform: PlatformId,
        reason: &'static str,
    },
    /// Shortcut capture requires an OS or browser permission that has not been granted.
    PermissionRequired {
        platform: PlatformId,
        permission: &'static str,
    },
}

impl fmt::Display for ShortcutCaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform { platform, reason } => {
                write!(
                    f,
                    "Shortcut capture unsupported on {}: {reason}",
                    platform.display_name()
                )
            }
            Self::PermissionRequired {
                platform,
                permission,
            } => {
                write!(
                    f,
                    "Shortcut capture on {} requires permission: {permission}",
                    platform.display_name()
                )
            }
        }
    }
}

impl std::error::Error for ShortcutCaptureError {}

/// Controller managing shortcut-capture toggle state and platform capability rules.
#[derive(Debug, Clone)]
pub struct ShortcutCaptureController {
    platform_row: PlatformCapabilityRow,
    mode: ShortcutCaptureMode,
    has_permission: bool,
}

impl ShortcutCaptureController {
    /// Create a controller with an explicit platform capability row.
    #[must_use]
    pub fn new(platform_row: PlatformCapabilityRow) -> Self {
        Self {
            platform_row,
            mode: ShortcutCaptureMode::Disabled,
            has_permission: false,
        }
    }

    /// Create a controller initialized for the current detected host platform.
    #[must_use]
    pub fn for_current_platform() -> Self {
        let platform = PlatformId::current();
        let row = PlatformCapabilityRow::for_platform(platform);
        Self::new(row)
    }

    /// Get the platform capability row.
    #[must_use]
    pub const fn platform_row(&self) -> &PlatformCapabilityRow {
        &self.platform_row
    }

    /// Current shortcut capture mode.
    #[must_use]
    pub const fn mode(&self) -> ShortcutCaptureMode {
        self.mode
    }

    /// True if shortcut capture is currently enabled.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.mode.is_enabled()
    }

    /// Check if permission has been recorded.
    #[must_use]
    pub const fn has_permission(&self) -> bool {
        self.has_permission
    }

    /// Update permission status (e.g. after user grants Accessibility or browser prompt).
    pub fn set_permission(&mut self, granted: bool) {
        self.has_permission = granted;
        if !granted
            && self.mode == ShortcutCaptureMode::Enabled
            && let ShortcutSupportStatus::SupportedWithPermission { .. } = self.platform_row.status
        {
            self.mode = ShortcutCaptureMode::Disabled;
            eprintln!(
                "[StateTrace: ShortcutCapture disabled due to revoked permission platform={:?}]",
                self.platform_row.platform
            );
        }
    }

    /// Validate whether enabling shortcut capture is permitted on this platform.
    pub fn can_enable(&self) -> Result<(), ShortcutCaptureError> {
        match self.platform_row.status {
            ShortcutSupportStatus::Supported => Ok(()),
            ShortcutSupportStatus::SupportedWithPermission { permission_name } => {
                if self.has_permission {
                    Ok(())
                } else {
                    Err(ShortcutCaptureError::PermissionRequired {
                        platform: self.platform_row.platform,
                        permission: permission_name,
                    })
                }
            }
            ShortcutSupportStatus::Unsupported { reason } => {
                Err(ShortcutCaptureError::UnsupportedPlatform {
                    platform: self.platform_row.platform,
                    reason,
                })
            }
        }
    }

    /// Explicitly set the capture mode.
    pub fn set_mode(
        &mut self,
        target: ShortcutCaptureMode,
    ) -> Result<ShortcutCaptureMode, ShortcutCaptureError> {
        if target == self.mode {
            return Ok(self.mode);
        }

        if target == ShortcutCaptureMode::Enabled {
            self.can_enable()?;
        }

        self.mode = target;
        let p = self.platform_row.platform;
        let m = self.platform_row.mechanism;
        eprintln!(
            "[StateTrace: ShortcutCapture toggled mode={target:?} platform={p:?} mechanism={m:?}]"
        );
        Ok(self.mode)
    }

    /// Toggle between Disabled and Enabled.
    pub fn toggle(&mut self) -> Result<ShortcutCaptureMode, ShortcutCaptureError> {
        let new_mode = match self.mode {
            ShortcutCaptureMode::Disabled => ShortcutCaptureMode::Enabled,
            ShortcutCaptureMode::Enabled => ShortcutCaptureMode::Disabled,
        };
        self.set_mode(new_mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_matrix_contains_all_seven_platforms() {
        let rows = PlatformCapabilityRow::all_platforms();
        assert_eq!(rows.len(), 7);

        let platforms: Vec<_> = rows.iter().map(|r| r.platform).collect();
        assert!(platforms.contains(&PlatformId::LinuxX11));
        assert!(platforms.contains(&PlatformId::LinuxWayland));
        assert!(platforms.contains(&PlatformId::Windows));
        assert!(platforms.contains(&PlatformId::MacOS));
        assert!(platforms.contains(&PlatformId::Browser));
        assert!(platforms.contains(&PlatformId::Android));
        assert!(platforms.contains(&PlatformId::Ios));
    }

    #[test]
    fn linux_x11_enables_cleanly_without_extra_permissions() {
        let row = PlatformCapabilityRow::for_platform(PlatformId::LinuxX11);
        let mut controller = ShortcutCaptureController::new(row);

        assert_eq!(controller.mode(), ShortcutCaptureMode::Disabled);
        assert!(!controller.is_enabled());

        let new_mode = controller.toggle().expect("linux x11 must toggle");
        assert_eq!(new_mode, ShortcutCaptureMode::Enabled);
        assert!(controller.is_enabled());

        let back = controller.toggle().expect("must toggle back");
        assert_eq!(back, ShortcutCaptureMode::Disabled);
        assert!(!controller.is_enabled());
    }

    #[test]
    fn macos_requires_accessibility_permission_to_enable() {
        let row = PlatformCapabilityRow::for_platform(PlatformId::MacOS);
        let mut controller = ShortcutCaptureController::new(row);

        // Without permission: must refuse
        let err = controller.toggle().unwrap_err();
        assert!(matches!(
            err,
            ShortcutCaptureError::PermissionRequired {
                platform: PlatformId::MacOS,
                ..
            }
        ));

        // Grant permission: now enables cleanly
        controller.set_permission(true);
        assert_eq!(controller.toggle().unwrap(), ShortcutCaptureMode::Enabled);

        // Revoking permission drops back to Disabled
        controller.set_permission(false);
        assert_eq!(controller.mode(), ShortcutCaptureMode::Disabled);
    }

    #[test]
    fn mobile_platforms_typed_refusal() {
        for platform in [PlatformId::Android, PlatformId::Ios] {
            let row = PlatformCapabilityRow::for_platform(platform);
            let mut controller = ShortcutCaptureController::new(row);

            let err = controller.toggle().unwrap_err();
            assert!(matches!(
                err,
                ShortcutCaptureError::UnsupportedPlatform { .. }
            ));
            assert_eq!(controller.mode(), ShortcutCaptureMode::Disabled);
        }
    }
}
