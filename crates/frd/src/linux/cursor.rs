//! Cursor mode gating and metadata handling for Linux hosting (plan §10.1, §11.4).
//!
//! Enforces:
//! 1. Cursor metadata is emitted only when `CursorMode::Metadata` was explicitly granted.
//! 2. If `CursorMode::Embedded` was granted, cursor is composited into video frames;
//!    out-of-band cursor metadata is forbidden to prevent drawing duplicate cursors.
//! 3. If `CursorMode::Hidden`, cursor is neither composited nor emitted.

use super::coordinates::CompositorPoint;
use core::fmt;

/// Cursor presentation mode supported/granted by the `ScreenCast` portal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum CursorMode {
    /// Cursor is not included in the video stream and not reported as metadata.
    Hidden = 1,
    /// Cursor is drawn directly into the video frame pixels by the compositor.
    Embedded = 2,
    /// Cursor is provided out-of-band as separate metadata/position/shape buffers.
    Metadata = 4,
}

impl CursorMode {
    #[must_use]
    pub const fn from_u32(val: u32) -> Option<Self> {
        match val {
            1 => Some(Self::Hidden),
            2 => Some(Self::Embedded),
            4 => Some(Self::Metadata),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_u32(self) -> u32 {
        self as u32
    }
}

/// Out-of-band cursor position, hotspot, and shape metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct CursorMetadata {
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub width: u32,
    pub height: u32,
    pub position: Option<CompositorPoint>,
    pub visible: bool,
}

impl CursorMetadata {
    #[must_use]
    pub const fn new(
        hotspot_x: i32,
        hotspot_y: i32,
        width: u32,
        height: u32,
        position: Option<CompositorPoint>,
        visible: bool,
    ) -> Self {
        Self {
            hotspot_x,
            hotspot_y,
            width,
            height,
            position,
            visible,
        }
    }
}

/// Typed refusals when querying or emitting cursor metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMetadataRefusal {
    /// Metadata mode was not granted by the portal (e.g. `Embedded` or `Hidden`).
    MetadataNotGranted(CursorMode),
    /// Cursor metadata is empty or uninitialized.
    NotAvailable,
}

impl fmt::Display for CursorMetadataRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MetadataNotGranted(mode) => {
                write!(
                    f,
                    "cursor metadata not granted by portal; current mode is {mode:?}"
                )
            }
            Self::NotAvailable => write!(f, "cursor metadata is not available"),
        }
    }
}

impl std::error::Error for CursorMetadataRefusal {}

/// Cursor manager tracking portal grant and current metadata.
pub struct CursorManager {
    granted_mode: CursorMode,
    current_metadata: Option<CursorMetadata>,
}

impl CursorManager {
    #[must_use]
    pub const fn new(granted_mode: CursorMode) -> Self {
        Self {
            granted_mode,
            current_metadata: None,
        }
    }

    #[must_use]
    pub const fn granted_mode(&self) -> CursorMode {
        self.granted_mode
    }

    /// Whether cursor metadata is allowed under the current grant.
    #[must_use]
    pub const fn allows_metadata(&self) -> bool {
        matches!(self.granted_mode, CursorMode::Metadata)
    }

    /// Update current cursor metadata if `CursorMode::Metadata` was granted.
    pub fn update_metadata(
        &mut self,
        metadata: CursorMetadata,
    ) -> Result<(), CursorMetadataRefusal> {
        if !self.allows_metadata() {
            return Err(CursorMetadataRefusal::MetadataNotGranted(self.granted_mode));
        }
        self.current_metadata = Some(metadata);
        Ok(())
    }

    /// Retrieve current cursor metadata if granted.
    pub fn get_metadata(&self) -> Result<&CursorMetadata, CursorMetadataRefusal> {
        if !self.allows_metadata() {
            return Err(CursorMetadataRefusal::MetadataNotGranted(self.granted_mode));
        }
        self.current_metadata
            .as_ref()
            .ok_or(CursorMetadataRefusal::NotAvailable)
    }
}
