//! Agent observation data models with validity boundaries and geometry generations.
//!
//! Per plan section 18.2:
//! - An agent observation includes display geometry, configuration generation, frame identity,
//!   source-freshness status, capture/presentation timestamps with uncertainty, and
//!   current control authority.
//! - An optional screenshot is an explicit user-requested artifact, not default JSON bloat.
//! - Strict privacy invariant: No window titles, application names, or clipboard dumps.

use super::envelope::RobotError;
use super::sha256::sha256_hex;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Evidence level backing an observation artifact.
///
/// Per plan section 18.2:
/// "Bind a screenshot/artifact to the observation that produced it and report
/// whether it was actually decoded, merely submitted to a compositor, or
/// instrumentally observed. Do not infer semantic success from a new frame number."
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceLevel {
    /// Frame was decoded by the local HEVC video decoder into raw raster memory.
    Decoded,
    /// Frame surface was submitted to native compositor / window manager presentation queue.
    SubmittedToCompositor,
    /// Frame presentation was instrumentally measured on the physical display (e.g. vblank / presentation timing counter).
    InstrumentallyObserved,
}

impl fmt::Display for EvidenceLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decoded => write!(f, "decoded"),
            Self::SubmittedToCompositor => write!(f, "submitted_to_compositor"),
            Self::InstrumentallyObserved => write!(f, "instrumentally_observed"),
        }
    }
}

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

/// User-requested observation artifact (such as a screenshot) bound to a producing observation.
///
/// Per plan section 18.2:
/// "An optional screenshot is a user-requested observation artifact, not a second interactive video transport codec."
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationArtifact {
    /// Unique artifact identifier (e.g. "art-frame-1204-a1b2c3d4e5f6").
    pub artifact_id: String,
    /// Monotonic frame serial of the producing observation.
    pub producing_frame_serial: u64,
    /// Host display geometry generation when this artifact was captured.
    pub producing_geometry_generation: u64,
    /// Capture timestamp in Unix epoch milliseconds.
    pub capture_timestamp_unix_ms: u64,
    /// Evidence level backing this artifact.
    pub evidence_level: EvidenceLevel,
    /// MIME type of the artifact (e.g. "image/png", "image/raw-rgba").
    pub mime_type: String,
    /// Image width in pixels.
    pub pixel_width: u32,
    /// Image height in pixels.
    pub pixel_height: u32,
    /// Size of artifact payload in bytes.
    pub byte_count: usize,
    /// Cryptographic SHA-256 digest of artifact payload (64 lowercase hex characters).
    pub sha256: String,
    /// Optional storage path or location on disk.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub storage_path: Option<String>,
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
    /// Presentation timestamp in Unix epoch milliseconds (if presented).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub presentation_timestamp_unix_ms: Option<u64>,
    /// Time synchronization and measurement uncertainty in milliseconds.
    pub uncertainty_ms: u64,
    /// Active control authority lease handle, if held by this caller.
    pub control_authority: Option<String>,
    /// Optional bound artifact requested by caller (screenshot / frame grab).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub artifact: Option<ObservationArtifact>,
}

impl RobotObservationData {
    /// Bind an existing artifact to this observation with strict provenance checks.
    ///
    /// Refuses with typed error if the artifact does not match this observation's
    /// frame serial, geometry generation, capture timestamp, or dimensions.
    pub fn bind_artifact(&mut self, artifact: ObservationArtifact) -> Result<(), RobotError> {
        if artifact.producing_frame_serial != self.frame_serial {
            return Err(RobotError::new(
                "artifact_frame_mismatch",
                format!(
                    "Artifact frame serial {} does not match observation frame serial {}.",
                    artifact.producing_frame_serial, self.frame_serial
                ),
            ));
        }
        if artifact.producing_geometry_generation != self.geometry_generation {
            return Err(RobotError::new(
                "artifact_geometry_mismatch",
                format!(
                    "Artifact geometry generation {} does not match observation generation {}.",
                    artifact.producing_geometry_generation, self.geometry_generation
                ),
            ));
        }
        if artifact.capture_timestamp_unix_ms != self.capture_timestamp_unix_ms {
            return Err(RobotError::new(
                "artifact_timestamp_mismatch",
                format!(
                    "Artifact capture timestamp {} does not match observation timestamp {}.",
                    artifact.capture_timestamp_unix_ms, self.capture_timestamp_unix_ms
                ),
            ));
        }
        if artifact.pixel_width != self.geometry.pixel_width
            || artifact.pixel_height != self.geometry.pixel_height
        {
            return Err(RobotError::new(
                "artifact_dimension_mismatch",
                format!(
                    "Artifact dimensions {}x{} do not match observation geometry {}x{}.",
                    artifact.pixel_width,
                    artifact.pixel_height,
                    self.geometry.pixel_width,
                    self.geometry.pixel_height
                ),
            ));
        }
        self.artifact = Some(artifact);
        Ok(())
    }

    /// Create and bind a screenshot artifact from raw pixel/image bytes.
    pub fn create_and_bind_screenshot(
        &mut self,
        evidence_level: EvidenceLevel,
        mime_type: impl Into<String>,
        raw_bytes: &[u8],
        storage_path: Option<String>,
    ) -> Result<ObservationArtifact, RobotError> {
        let hash = sha256_hex(raw_bytes);
        let prefix = if hash.len() >= 12 { &hash[..12] } else { &hash };
        let artifact = ObservationArtifact {
            artifact_id: format!("art-frame-{}-{}", self.frame_serial, prefix),
            producing_frame_serial: self.frame_serial,
            producing_geometry_generation: self.geometry_generation,
            capture_timestamp_unix_ms: self.capture_timestamp_unix_ms,
            evidence_level,
            mime_type: mime_type.into(),
            pixel_width: self.geometry.pixel_width,
            pixel_height: self.geometry.pixel_height,
            byte_count: raw_bytes.len(),
            sha256: hash,
            storage_path,
        };
        self.bind_artifact(artifact.clone())?;
        Ok(artifact)
    }

    /// Render human-readable observation summary.
    pub fn render_human(&self) -> String {
        use std::fmt::Write as _;
        let mut out = format!(
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
        );
        if let Some(pres) = self.presentation_timestamp_unix_ms {
            let _ = writeln!(out, "  Presentation: {pres} ms");
        }
        if let Some(art) = &self.artifact {
            let _ = writeln!(
                out,
                "  Artifact: {} (evidence: {}, {}x{}, {} bytes, sha256: {}...)",
                art.artifact_id,
                art.evidence_level,
                art.pixel_width,
                art.pixel_height,
                art.byte_count,
                &art.sha256[..12]
            );
            if let Some(path) = &art.storage_path {
                let _ = writeln!(out, "    Path: {path}");
            }
        }
        out
    }
}
