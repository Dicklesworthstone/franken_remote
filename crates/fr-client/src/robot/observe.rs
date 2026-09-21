//! Agent observation data models with validity boundaries and geometry generations.
//!
//! Per plan section 18.2:
//! - An agent observation includes display geometry, configuration generation, frame identity,
//!   source-freshness status, capture/presentation timestamps with uncertainty, and
//!   current control authority.
//! - An optional screenshot is an explicit user-requested artifact, not default JSON bloat.
//! - Strict privacy invariant: No window titles, application names, or clipboard dumps.

use serde::{Deserialize, Serialize};

/// Rational scale and pixel dimensions of the observed display.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayGeometryInfo {
    /// Zero-based physical or virtual display index.
    pub display_index: u32,
    /// Physical width in pixels.
    pub pixel_width: u32,
    /// Physical height in pixels.
    pub pixel_height: u32,
    /// Logical width in desktop coordinates.
    pub logical_width: u32,
    /// Logical height in desktop coordinates.
    pub logical_height: u32,
    /// Rational scale factor numerator (e.g. 2 for 200%, 5 for 125%).
    pub scale_numerator: u32,
    /// Rational scale factor denominator (e.g. 1 for 200%, 4 for 125%).
    pub scale_denominator: u32,
    /// Display rotation in clockwise degrees (0, 90, 180, 270).
    pub rotation_degrees: u32,
}

/// Observation payload returned to the agent with explicit validity boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RobotObservationData {
    /// Remote host identity.
    pub host: String,
    /// Observed display geometry.
    pub geometry: DisplayGeometryInfo,
    /// Monotonic generation counter of the host display arrangement.
    pub geometry_generation: u64,
    /// Monotonic generation of the active HEVC codec configuration.
    pub configuration_generation: u64,
    /// Verified monotonic frame sequence number from host capture pipeline.
    pub frame_serial: u64,
    /// Provenance freshness: "fresh", "`idle_verified`", or "stale".
    pub source_freshness: String,
    /// Capture timestamp in Unix epoch milliseconds.
    pub capture_timestamp_unix_ms: u64,
    /// Time synchronization and measurement uncertainty in milliseconds.
    pub uncertainty_ms: u64,
    /// Active control authority lease handle, if held by this caller.
    pub control_authority: Option<String>,
}

impl RobotObservationData {
    /// Render human-readable observation summary.
    pub fn render_human(&self) -> String {
        format!(
            "Observation: {}\n  Display: {} ({}x{} phys, {}x{} log, scale {}/{})\n  Geometry Gen: {}\n  Configuration Gen: {}\n  Frame Serial: {}\n  Freshness: {}\n  Captured: {} ms (±{} ms)\n  Control: {}\n",
            self.host,
            self.geometry.display_index,
            self.geometry.pixel_width,
            self.geometry.pixel_height,
            self.geometry.logical_width,
            self.geometry.logical_height,
            self.geometry.scale_numerator,
            self.geometry.scale_denominator,
            self.geometry_generation,
            self.configuration_generation,
            self.frame_serial,
            self.source_freshness,
            self.capture_timestamp_unix_ms,
            self.uncertainty_ms,
            self.control_authority.as_deref().unwrap_or("none")
        )
    }
}
