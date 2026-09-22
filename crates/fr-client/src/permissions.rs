//! Platform permission models and user remediation guidance for desktop clients (Plan §16.1).
#![forbid(unsafe_code)]

use crate::shortcut::PlatformId;

/// Categorized platform permission required by desktop client shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionCategory {
    /// macOS Accessibility TCC or Windows low-level keyboard hook for shortcut capture.
    SystemShortcuts,
    /// Host-side screen recording permission (macOS TCC Screen Recording / Wayland Portal).
    ScreenCapture,
    /// Client microphone input permission for audio uplink.
    Microphone,
    /// Remote input injection authorization (Windows UIPI / Wayland virtual input).
    InputInjection,
}

/// Current status of a platform capability permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionState {
    Granted,
    Denied,
    NotDetermined,
    Restricted,
}

/// Diagnostic permission report explaining what is required and how to fix it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionExplanation {
    pub platform: PlatformId,
    pub category: PermissionCategory,
    pub state: PermissionState,
    pub title: &'static str,
    pub explanation: &'static str,
    pub remediation_steps: &'static str,
}

impl PermissionExplanation {
    /// Generate platform-specific permission explanation and instructions.
    pub fn for_category(
        platform: PlatformId,
        category: PermissionCategory,
        state: PermissionState,
    ) -> Self {
        match (platform, category) {
            (PlatformId::MacOS, PermissionCategory::SystemShortcuts) => Self {
                platform,
                category,
                state,
                title: "macOS Accessibility Permission Required",
                explanation: "Intercepting macOS global shortcuts (Cmd+Tab, Spotlight) requires Accessibility permission.",
                remediation_steps: "Open System Settings > Privacy & Security > Accessibility, and enable FrankenRemote.",
            },
            (PlatformId::MacOS, PermissionCategory::ScreenCapture) => Self {
                platform,
                category,
                state,
                title: "macOS Screen Recording Permission",
                explanation: "Hosting a desktop session requires macOS Screen Recording permission.",
                remediation_steps: "Open System Settings > Privacy & Security > Screen Recording, and enable frd.",
            },
            (PlatformId::Windows, PermissionCategory::SystemShortcuts) => Self {
                platform,
                category,
                state,
                title: "Windows Shortcut Capture",
                explanation: "Capturing Windows system keys (Win key, Alt+Tab) uses low-level keyboard hooks.",
                remediation_steps: "If running against elevated applications, launch FrankenRemote as Administrator.",
            },
            (
                PlatformId::LinuxX11 | PlatformId::LinuxWayland,
                PermissionCategory::SystemShortcuts,
            ) => Self {
                platform,
                category,
                state,
                title: "Linux Shortcut Capture",
                explanation: "Capturing system shortcuts requires window focus and compositor grab support.",
                remediation_steps: "Ensure the FrankenRemote window has active keyboard focus.",
            },
            (_, PermissionCategory::Microphone) => Self {
                platform,
                category,
                state,
                title: "Microphone Access",
                explanation: "Transmitting audio to the remote host requires microphone authorization.",
                remediation_steps: "Enable microphone permissions in your operating system privacy settings.",
            },
            (_, PermissionCategory::InputInjection) => Self {
                platform,
                category,
                state,
                title: "Input Authority Injection",
                explanation: "Host input submission requires authorization from the active desktop session.",
                remediation_steps: "Verify that frd is running in the authenticated user desktop session.",
            },
            _ => Self {
                platform,
                category,
                state,
                title: "Permission Required",
                explanation: "This desktop platform requires authorization for the requested capability.",
                remediation_steps: "Check system permissions and user settings.",
            },
        }
    }
}
