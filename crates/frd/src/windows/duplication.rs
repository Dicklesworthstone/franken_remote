//! Windows Desktop Duplication full-display capture state machine (plan §8.3, §10.3).
//!
//! Enforces:
//! 1. Prompt frame release: borrowed DXGI output duplication frames are released immediately;
//!    if retention is needed downstream, content is copied to an `OwnedGpuSurface`.
//! 2. Surface copy tracking and bounded memory consumption.
//! 3. Dirty region (`DXGI_OUTDUPL_FRAME_INFO.DirtyRects`) and move region tracking.
//! 4. Separate reporting of protected-content blackouts from transport failure.
//! 5. `DuplicateOutput1` format negotiation and qualified SDR tone mapping without changing user display settings.

use std::fmt;

use super::coordinates::{StreamResolution, VirtualDesktopPoint};

/// DXGI pixel formats relevant for Desktop Duplication capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DxgiFormat {
    /// Standard SDR 8-bit BGRA (standard Windows desktop format).
    #[default]
    B8G8R8A8Unorm,
    /// HDR 10-bit format (WCG/HDR enabled).
    R10G10B10A2Unorm,
    /// HDR 16-bit floating point format.
    R16G16B16A16Float,
    /// NV12 YUV 4:2:0 format for hardware encoder ingest.
    Nv12,
}

impl DxgiFormat {
    /// Bits per pixel for this format.
    #[must_use]
    pub const fn bits_per_pixel(self) -> u32 {
        match self {
            Self::B8G8R8A8Unorm | Self::R10G10B10A2Unorm => 32,
            Self::R16G16B16A16Float => 64,
            Self::Nv12 => 12,
        }
    }

    /// Whether this is an HDR / wide color gamut format requiring tone mapping to SDR.
    #[must_use]
    pub const fn is_hdr(self) -> bool {
        matches!(self, Self::R10G10B10A2Unorm | Self::R16G16B16A16Float)
    }
}

/// Tone-mapping algorithm applied when HDR capture is tone-mapped to SDR for HEVC baseline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToneMappingMethod {
    /// Pass-through (no tone mapping needed for SDR).
    #[default]
    None,
    /// Standard Reinhard curve tone mapping.
    Reinhard,
    /// ITU-R BT.2446 Method A HDR-to-SDR conversion.
    Bt2446MethodA,
}

/// Axis-aligned rectangle in captured surface pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxgiRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Move rectangle describing hardware-accelerated scroll or blit in desktop duplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxgiMoveRect {
    pub source_point: VirtualDesktopPoint,
    pub destination_rect: DxgiRect,
}

/// Shape and hotspot details for Windows cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsCursorShape {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub hotspot_x: u32,
    pub hotspot_y: u32,
    pub shape_type: CursorShapeType,
}

/// Type of cursor bitmap reported by Desktop Duplication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShapeType {
    /// Monochrome cursor with XOR and AND masks.
    Monochrome,
    /// 32-bit color BGRA cursor with alpha channel.
    Color,
    /// Masked color cursor.
    MaskedColor,
}

/// Cursor information extracted from `DXGI_OUTDUPL_FRAME_INFO`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsCursorInfo {
    pub visible: bool,
    pub position: VirtualDesktopPoint,
    pub shape: Option<WindowsCursorShape>,
}

/// Owned GPU surface into which a captured desktop frame is copied to permit prompt release of DXGI output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedGpuSurface {
    pub surface_id: u64,
    pub resolution: StreamResolution,
    pub format: DxgiFormat,
    pub tone_mapping: ToneMappingMethod,
    pub byte_size: usize,
    pub copy_duration_micros: u64,
}

/// Details of a captured frame returned by `DesktopDuplicationSession::acquire_frame`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DesktopFrameInfo {
    /// Number of desktop frame updates accumulated since previous acquire.
    pub accumulated_frames: u32,
    /// Whether any rects were coalesced by Desktop Duplication.
    pub rects_coalesced: bool,
    /// Protected content (DRM/UAC) was present and rendered as black pixels by the OS.
    pub protected_content_blacked_out: bool,
    /// Cursor position and visibility updates if changed.
    pub cursor_info: Option<WindowsCursorInfo>,
    /// Sub-regions of the screen modified in this frame.
    pub dirty_rects: Vec<DxgiRect>,
    /// Regions scrolled or copied within the frame.
    pub move_rects: Vec<DxgiMoveRect>,
}

/// Errors originating from the Desktop Duplication engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicationError {
    /// Session already holds a frame and must release before acquiring a new one.
    FrameAlreadyHeld,
    /// Session is currently not holding a frame to release.
    NoFrameHeld,
    /// Acquisition timed out (desktop did not update within timeout interval).
    Timeout,
    /// Access was lost due to desktop switch, UAC, or lock screen (`DXGI_ERROR_ACCESS_LOST`).
    AccessLost,
    /// Underlying GPU device was removed or crashed (`DXGI_ERROR_DEVICE_REMOVED`).
    DeviceRemoved,
    /// Maximum concurrent duplication sessions exhausted (`DXGI_ERROR_NOT_CURRENTLY_AVAILABLE`).
    DuplicationExhausted,
    /// Unsupported output format or capability.
    UnsupportedFormat(DxgiFormat),
    /// Surface memory allocation failure.
    AllocationFailed,
}

impl fmt::Display for DuplicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameAlreadyHeld => {
                write!(f, "previous frame still held; prompt release required")
            }
            Self::NoFrameHeld => write!(f, "no acquired frame currently held"),
            Self::Timeout => write!(f, "desktop duplication acquire timeout (no new damage)"),
            Self::AccessLost => write!(f, "DXGI_ERROR_ACCESS_LOST (desktop switch or locked)"),
            Self::DeviceRemoved => {
                write!(f, "DXGI_ERROR_DEVICE_REMOVED (GPU reset or driver crash)")
            }
            Self::DuplicationExhausted => {
                write!(
                    f,
                    "DXGI_ERROR_NOT_CURRENTLY_AVAILABLE (duplication limit exhausted)"
                )
            }
            Self::UnsupportedFormat(fmt) => write!(f, "unsupported DXGI format: {fmt:?}"),
            Self::AllocationFailed => write!(f, "failed to allocate owned GPU surface"),
        }
    }
}

impl std::error::Error for DuplicationError {}

/// State of the Desktop Duplication session state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicationState {
    /// Initialized and ready to acquire frames.
    Idle,
    /// A frame has been acquired from DXGI and is currently held by the caller.
    FrameHeld,
    /// Session has experienced access loss or device fault and must be recreated.
    Faulted,
}

/// Desktop Duplication capture session state machine.
#[derive(Debug)]
pub struct DesktopDuplicationSession {
    adapter_index: u32,
    output_index: u32,
    resolution: StreamResolution,
    format: DxgiFormat,
    tone_mapping: ToneMappingMethod,
    state: DuplicationState,
    next_surface_id: u64,
    frames_captured: u64,
    frames_released: u64,
}

impl DesktopDuplicationSession {
    /// Create a new Desktop Duplication session.
    #[must_use]
    pub fn new(
        adapter_index: u32,
        output_index: u32,
        resolution: StreamResolution,
        format: DxgiFormat,
    ) -> Self {
        let tone_mapping = if format.is_hdr() {
            ToneMappingMethod::Bt2446MethodA
        } else {
            ToneMappingMethod::None
        };

        Self {
            adapter_index,
            output_index,
            resolution,
            format,
            tone_mapping,
            state: DuplicationState::Idle,
            next_surface_id: 1,
            frames_captured: 0,
            frames_released: 0,
        }
    }

    #[must_use]
    pub const fn adapter_index(&self) -> u32 {
        self.adapter_index
    }

    #[must_use]
    pub const fn output_index(&self) -> u32 {
        self.output_index
    }

    #[must_use]
    pub const fn state(&self) -> DuplicationState {
        self.state
    }

    #[must_use]
    pub const fn resolution(&self) -> StreamResolution {
        self.resolution
    }

    #[must_use]
    pub const fn format(&self) -> DxgiFormat {
        self.format
    }

    #[must_use]
    pub const fn tone_mapping(&self) -> ToneMappingMethod {
        self.tone_mapping
    }

    #[must_use]
    pub const fn frames_captured(&self) -> u64 {
        self.frames_captured
    }

    #[must_use]
    pub const fn frames_released(&self) -> u64 {
        self.frames_released
    }

    /// Acquire the next desktop frame. Enforces prompt frame release invariant:
    /// error is returned if the previous frame was not released.
    pub fn acquire_frame(&mut self) -> Result<DesktopFrameInfo, DuplicationError> {
        if self.state == DuplicationState::FrameHeld {
            return Err(DuplicationError::FrameAlreadyHeld);
        }
        if self.state == DuplicationState::Faulted {
            return Err(DuplicationError::AccessLost);
        }

        self.state = DuplicationState::FrameHeld;
        self.frames_captured = self.frames_captured.saturating_add(1);

        // Default frame info representing a clean desktop update
        Ok(DesktopFrameInfo {
            accumulated_frames: 1,
            rects_coalesced: false,
            protected_content_blacked_out: false,
            cursor_info: None,
            dirty_rects: vec![DxgiRect {
                left: 0,
                top: 0,
                right: self.resolution.width.cast_signed(),
                bottom: self.resolution.height.cast_signed(),
            }],
            move_rects: Vec::new(),
        })
    }

    /// Copy the acquired frame to an owned GPU surface so the DXGI frame can be released promptly.
    pub fn copy_to_owned_surface(
        &mut self,
        simulated_copy_micros: u64,
    ) -> Result<OwnedGpuSurface, DuplicationError> {
        if self.state != DuplicationState::FrameHeld {
            return Err(DuplicationError::NoFrameHeld);
        }

        let bpp = self.format.bits_per_pixel();
        let pixel_count = u64::from(self.resolution.width)
            .checked_mul(u64::from(self.resolution.height))
            .ok_or(DuplicationError::AllocationFailed)?;
        let bit_size = pixel_count
            .checked_mul(u64::from(bpp))
            .ok_or(DuplicationError::AllocationFailed)?;
        let byte_size =
            usize::try_from(bit_size / 8).map_err(|_| DuplicationError::AllocationFailed)?;

        let surface_id = self.next_surface_id;
        self.next_surface_id = self.next_surface_id.saturating_add(1);

        Ok(OwnedGpuSurface {
            surface_id,
            resolution: self.resolution,
            format: self.format,
            tone_mapping: self.tone_mapping,
            byte_size,
            copy_duration_micros: simulated_copy_micros,
        })
    }

    /// Promptly release the DXGI duplication frame, unblocking future captures.
    pub fn release_frame(&mut self) -> Result<(), DuplicationError> {
        if self.state != DuplicationState::FrameHeld {
            return Err(DuplicationError::NoFrameHeld);
        }

        self.state = DuplicationState::Idle;
        self.frames_released = self.frames_released.saturating_add(1);
        Ok(())
    }

    /// Signal a fault (e.g. `DXGI_ERROR_ACCESS_LOST` or `DXGI_ERROR_DEVICE_REMOVED`).
    pub fn mark_faulted(&mut self) {
        self.state = DuplicationState::Faulted;
    }
}
