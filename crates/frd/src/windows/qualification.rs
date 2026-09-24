//! Windows host qualification matrix and hardware evidence records (plan §8.3, §10.3, §23 Phase 2).
//!
//! Enforces:
//! 1. Recording tested Windows OS versions (Windows 11 24H2/23H2, Windows 10 22H2, Windows Server 2022).
//! 2. Recording tested GPU hardware encoders (NVIDIA NVENC, Intel QSV, AMD AMF).
//! 3. Surfacing UIPI elevated-window limitation as a typed capability, never an unexpected failure.
//! 4. Transparently documenting Desktop Duplication session limits and HDR tone-mapping capabilities.

use super::hybrid_gpu::HardwareEncoderKind;

/// Qualification outcome for a specific capability row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualificationStatus {
    /// Verified working with recorded hardware test evidence.
    Passed,
    /// Verified not working or unsupported by design/platform limit.
    Failed,
    /// Blocked by missing hardware or driver dependency.
    Blocked,
    /// Not yet tested on this exact configuration.
    NotTested,
}

/// Windows OS release family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsReleaseFamily {
    Windows11_24H2,
    Windows11_23H2,
    Windows10_22H2,
    WindowsServer2022,
    UnsupportedLegacy,
}

/// Row in the Windows host qualification matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsQualificationRow {
    pub os_family: WindowsReleaseFamily,
    pub build_number: u32,
    pub gpu_description: &'static str,
    pub driver_version: &'static str,
    pub desktop_duplication: QualificationStatus,
    pub duplicate_output1_hdr: QualificationStatus,
    pub hardware_hevc: (HardwareEncoderKind, QualificationStatus),
    pub session0_isolation_verified: QualificationStatus,
    pub uipi_elevation_refusal_typed: QualificationStatus,
}

/// Qualified Windows configurations: none. Rows claiming RTX 4080 NVENC, Arc
/// A770 QSV, RX 7900 XTX AMF and RTX A4000 hosts as Passed were withdrawn on
/// 2026-09-24: no Windows capture or encode path exists and no such hardware
/// was tested. A row may be added only with retained evidence from a real run.
pub static QUALIFIED_WINDOWS_ROWS: &[WindowsQualificationRow] = &[];

/// Evaluate a host's build number to identify its release family.
#[must_use]
pub const fn classify_build_number(build: u32) -> WindowsReleaseFamily {
    if build >= 26100 {
        WindowsReleaseFamily::Windows11_24H2
    } else if build >= 22000 {
        WindowsReleaseFamily::Windows11_23H2
    } else if build >= 19041 {
        WindowsReleaseFamily::Windows10_22H2
    } else if build >= 17763 {
        WindowsReleaseFamily::WindowsServer2022
    } else {
        WindowsReleaseFamily::UnsupportedLegacy
    }
}
