//! Linux compositor qualification matrix and capability evaluator (plan §10.1, §23 Phase 2, §24.2).
//!
//! Linux hosting spans compositors with genuinely different capability sets.
//! In accordance with plan §10.1:
//! - Minimum qualification table tracks GNOME, KDE, and Hyprland/wlroots separately with exact versions.
//! - Independent evaluations for: capture, pointer, keyboard, restore token, clipboard, playback audio.
//! - Unsupported rows are actionable refusals or an explicit view-only capability;
//!   an unsupported or denied input portal never triggers a privileged input fallback (e.g. uinput/XTest).

/// Compositor family running on the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CompositorFamily {
    GnomeMutter,
    KdeKWin,
    HyprlandWlroots,
    Other(String),
}

impl CompositorFamily {
    #[must_use]
    pub fn from_desktop_name(desktop: &str) -> Self {
        let upper = desktop.to_ascii_uppercase();
        if upper.contains("GNOME") {
            Self::GnomeMutter
        } else if upper.contains("KDE") {
            Self::KdeKWin
        } else if upper.contains("HYPRLAND") || upper.contains("SWAY") || upper.contains("WLROOTS")
        {
            Self::HyprlandWlroots
        } else {
            Self::Other(desktop.to_string())
        }
    }

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::GnomeMutter => "GNOME (Mutter)",
            Self::KdeKWin => "KDE Plasma (KWin)",
            Self::HyprlandWlroots => "Hyprland / wlroots",
            Self::Other(_) => "Other / Unknown",
        }
    }
}

/// Qualification test status for a single capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualificationStatus {
    /// Fully qualified and tested with green fault suite.
    Passed,
    /// Explicitly refused with a documented reason.
    Refused(&'static str),
    /// Not tested in this matrix row.
    Untested,
}

impl QualificationStatus {
    #[must_use]
    pub const fn is_passed(self) -> bool {
        matches!(self, Self::Passed)
    }

    #[must_use]
    pub const fn refusal_reason(self) -> Option<&'static str> {
        match self {
            Self::Refused(reason) => Some(reason),
            _ => None,
        }
    }
}

/// A tested qualification row with recorded component versions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompositorQualificationRow {
    pub family: CompositorFamily,
    pub compositor_version: &'static str,
    pub portal_backend: &'static str,
    pub pipewire_version: &'static str,
    pub capture: QualificationStatus,
    pub pointer: QualificationStatus,
    pub keyboard: QualificationStatus,
    pub restore_token: QualificationStatus,
    pub clipboard: QualificationStatus,
    pub playback_audio: QualificationStatus,
}

/// Published qualification matrix rows.
pub static QUALIFIED_ROWS: &[CompositorQualificationRow] = &[
    CompositorQualificationRow {
        family: CompositorFamily::GnomeMutter,
        compositor_version: "Mutter 46.4 / 47.0",
        portal_backend: "xdg-desktop-portal-gnome 46.2 / 47.0",
        pipewire_version: "PipeWire 1.2.0+",
        capture: QualificationStatus::Passed,
        pointer: QualificationStatus::Passed,
        keyboard: QualificationStatus::Passed,
        restore_token: QualificationStatus::Passed,
        clipboard: QualificationStatus::Passed,
        playback_audio: QualificationStatus::Passed,
    },
    CompositorQualificationRow {
        family: CompositorFamily::KdeKWin,
        compositor_version: "KWin 6.1.5 / 6.2.0",
        portal_backend: "xdg-desktop-portal-kde 6.1.5 / 6.2.0",
        pipewire_version: "PipeWire 1.2.0+",
        capture: QualificationStatus::Passed,
        pointer: QualificationStatus::Passed,
        keyboard: QualificationStatus::Passed,
        restore_token: QualificationStatus::Passed,
        clipboard: QualificationStatus::Passed,
        playback_audio: QualificationStatus::Passed,
    },
    CompositorQualificationRow {
        family: CompositorFamily::HyprlandWlroots,
        compositor_version: "Hyprland 0.42.0",
        portal_backend: "xdg-desktop-portal-hyprland 1.3.3",
        pipewire_version: "PipeWire 1.2.0+",
        capture: QualificationStatus::Passed,
        pointer: QualificationStatus::Refused(
            "xdg-desktop-portal-hyprland lacks RemoteDesktop / EIS input implementation; screen capture is supported view-only",
        ),
        keyboard: QualificationStatus::Refused(
            "xdg-desktop-portal-hyprland lacks RemoteDesktop / EIS input implementation; screen capture is supported view-only",
        ),
        restore_token: QualificationStatus::Passed,
        clipboard: QualificationStatus::Passed,
        playback_audio: QualificationStatus::Passed,
    },
];

/// Resulting capability level evaluated for a host compositor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositorCapability {
    /// Full interactive remote workstation (capture + pointer + keyboard + clipboard).
    FullControl,
    /// Explicit view-only workstation: screen observation is allowed, input is refused with typed reason.
    ViewOnly { input_refusal_reason: &'static str },
    /// Unsupported compositor environment: session refused.
    Unsupported { detail: String },
}

/// Evaluate compositor environment against the qualification table.
#[must_use]
pub fn evaluate_compositor(family: &CompositorFamily) -> CompositorCapability {
    let row = QUALIFIED_ROWS.iter().find(|r| &r.family == family);

    match row {
        Some(row) => {
            if !row.capture.is_passed() {
                return CompositorCapability::Unsupported {
                    detail: format!("capture not supported on {}", family.as_str()),
                };
            }

            if row.pointer.is_passed() && row.keyboard.is_passed() {
                CompositorCapability::FullControl
            } else {
                let reason = row
                    .pointer
                    .refusal_reason()
                    .or_else(|| row.keyboard.refusal_reason())
                    .unwrap_or("input injection is not supported on this compositor");

                CompositorCapability::ViewOnly {
                    input_refusal_reason: reason,
                }
            }
        }
        None => CompositorCapability::Unsupported {
            detail: format!(
                "compositor '{}' is not in the Linux qualification matrix",
                family.as_str()
            ),
        },
    }
}
