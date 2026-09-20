//! Host and client media pipeline policy, queue accounting, cursor management,
//! damage reconstruction, and idle state machine (Plan §11.1–§11.4, §13.4).

use core::fmt;
use fr_core::ids::DisplayGeometryGeneration;
use fr_wire::{CursorPosition, CursorShape, SourceObservation, WireError};
use std::collections::{HashMap, HashSet, VecDeque};

/// All distinct stages of the media pipeline (7 primary stages + 5 hidden stages).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StageKind {
    /// 1. Capture admission: one replaceable latest pending capture.
    CaptureAdmission,
    /// 2. Encoder submission: bounded backend-qualified in-flight surfaces (initially 1-2).
    EncoderSubmission,
    /// 3. Compressed outbound work: byte and deadline limited.
    CompressedOutbound,
    /// 4. Frame reassembly / dependency retention: negotiated window (2-12 units by count AND bytes).
    ReassemblyWindow,
    /// 5. Decoder submission: bounded queue respecting dependencies.
    DecoderSubmission,
    /// 6. Presentation: newest ready frame, discard obsolete presentation work.
    Presentation,
    /// 7. Audio: small adaptive jitter buffer with strict ceiling.
    AudioJitter,
    /// Hidden 8. Codec-internal surfaces owned by backend hardware.
    CodecInternalSurfaces,
    /// Hidden 9. Packet caches for selective repair retransmission.
    PacketCache,
    /// Hidden 10. QUIC/WSS send transport buffers.
    TransportSendBuffer,
    /// Hidden 11. Renderer-held frames in the display compositor.
    RendererHeldFrames,
    /// Hidden 12. Shared-viewer retention (subscribers charged for work they force).
    SharedViewerRetention,
}

impl fmt::Display for StageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CaptureAdmission => write!(f, "CaptureAdmission"),
            Self::EncoderSubmission => write!(f, "EncoderSubmission"),
            Self::CompressedOutbound => write!(f, "CompressedOutbound"),
            Self::ReassemblyWindow => write!(f, "ReassemblyWindow"),
            Self::DecoderSubmission => write!(f, "DecoderSubmission"),
            Self::Presentation => write!(f, "Presentation"),
            Self::AudioJitter => write!(f, "AudioJitter"),
            Self::CodecInternalSurfaces => write!(f, "CodecInternalSurfaces"),
            Self::PacketCache => write!(f, "PacketCache"),
            Self::TransportSendBuffer => write!(f, "TransportSendBuffer"),
            Self::RendererHeldFrames => write!(f, "RendererHeldFrames"),
            Self::SharedViewerRetention => write!(f, "SharedViewerRetention"),
        }
    }
}

/// Errors returned by the media pipeline components.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PipelineError {
    /// The count limit for a stage was exceeded.
    CountLimitExceeded {
        stage: StageKind,
        current: usize,
        limit: usize,
    },
    /// The byte limit for a stage was exceeded.
    ByteLimitExceeded {
        stage: StageKind,
        current: usize,
        limit: usize,
    },
    /// An attempt was made to prematurely release a surface still owned by the driver/GPU.
    DriverSurfaceOwnershipViolation { surface_id: u64 },
    /// Surface cannot be encoded because uninitialized pixels remain.
    SurfaceNotInitialized,
    /// Coordinate or rectangle exceeds the surface dimensions.
    OutOfBoundsDamage {
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        surface_width: u32,
        surface_height: u32,
    },
    /// Position update sequence was stale or regressed.
    StaleSequence { sequence: u64, latest: u64 },
    /// Mismatched geometry generation.
    MismatchedGeometry {
        expected: DisplayGeometryGeneration,
        actual: DisplayGeometryGeneration,
    },
    /// Capture pipeline is stalled or hung.
    CaptureStalled { last_observation_age_us: u64 },
    /// General wire protocol error.
    Wire(WireError),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CountLimitExceeded {
                stage,
                current,
                limit,
            } => write!(
                f,
                "{stage} count limit exceeded: current {current}, limit {limit}"
            ),
            Self::ByteLimitExceeded {
                stage,
                current,
                limit,
            } => write!(
                f,
                "{stage} byte limit exceeded: current {current}, limit {limit}"
            ),
            Self::DriverSurfaceOwnershipViolation { surface_id } => write!(
                f,
                "cannot release driver-owned surface {surface_id} prematurely"
            ),
            Self::SurfaceNotInitialized => {
                write!(f, "full surface reconstruction incomplete before encode")
            }
            Self::OutOfBoundsDamage {
                x,
                y,
                width,
                height,
                surface_width,
                surface_height,
            } => write!(
                f,
                "damage rect [{x},{y} {width}x{height}] exceeds surface {surface_width}x{surface_height}"
            ),
            Self::StaleSequence { sequence, latest } => {
                write!(f, "stale sequence {sequence} <= latest {latest}")
            }
            Self::MismatchedGeometry { expected, actual } => write!(
                f,
                "geometry generation mismatch: expected {expected:?}, actual {actual:?}"
            ),
            Self::CaptureStalled {
                last_observation_age_us,
            } => write!(
                f,
                "capture stalled: last observation was {last_observation_age_us}us ago"
            ),
            Self::Wire(e) => write!(f, "wire error: {e}"),
        }
    }
}

impl core::error::Error for PipelineError {}

/// Strict limits for one pipeline stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageLimits {
    pub max_count: usize,
    pub max_bytes: usize,
}

impl StageLimits {
    pub const fn new(max_count: usize, max_bytes: usize) -> Self {
        Self {
            max_count,
            max_bytes,
        }
    }
}

/// Dynamic usage and high-water marks for one pipeline stage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StageUsage {
    pub current_count: usize,
    pub current_bytes: usize,
    pub high_water_count: usize,
    pub high_water_bytes: usize,
}

/// Complete pipeline queue policy across all 12 stages (Plan §11.2).
#[derive(Debug, Clone)]
pub struct PipelineQueuePolicy {
    limits: HashMap<StageKind, StageLimits>,
}

impl Default for PipelineQueuePolicy {
    fn default() -> Self {
        let mut limits = HashMap::new();
        // 1. Capture admission: exactly 1 replaceable latest capture
        limits.insert(
            StageKind::CaptureAdmission,
            StageLimits::new(1, 64 * 1024 * 1024),
        );
        // 2. Encoder submission: 2 in-flight surfaces maximum
        limits.insert(
            StageKind::EncoderSubmission,
            StageLimits::new(2, 64 * 1024 * 1024),
        );
        // 3. Compressed outbound: e.g. 16 frames, 4 MiB
        limits.insert(
            StageKind::CompressedOutbound,
            StageLimits::new(16, 4 * 1024 * 1024),
        );
        // 4. Reassembly window: 12 units negotiated, 8 MiB
        limits.insert(
            StageKind::ReassemblyWindow,
            StageLimits::new(12, 8 * 1024 * 1024),
        );
        // 5. Decoder submission: 8 units, 16 MiB
        limits.insert(
            StageKind::DecoderSubmission,
            StageLimits::new(8, 16 * 1024 * 1024),
        );
        // 6. Presentation: 1-2 newest ready frames
        limits.insert(
            StageKind::Presentation,
            StageLimits::new(2, 32 * 1024 * 1024),
        );
        // 7. Audio jitter: small ceiling (e.g. 50 packets, 64 KiB)
        limits.insert(StageKind::AudioJitter, StageLimits::new(50, 64 * 1024));
        // Hidden stages:
        limits.insert(
            StageKind::CodecInternalSurfaces,
            StageLimits::new(4, 64 * 1024 * 1024),
        );
        limits.insert(
            StageKind::PacketCache,
            StageLimits::new(2048, 8 * 1024 * 1024),
        );
        limits.insert(
            StageKind::TransportSendBuffer,
            StageLimits::new(512, 2 * 1024 * 1024),
        );
        limits.insert(
            StageKind::RendererHeldFrames,
            StageLimits::new(2, 32 * 1024 * 1024),
        );
        limits.insert(
            StageKind::SharedViewerRetention,
            StageLimits::new(8, 16 * 1024 * 1024),
        );
        Self { limits }
    }
}

impl PipelineQueuePolicy {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_limit(&mut self, stage: StageKind, limits: StageLimits) {
        self.limits.insert(stage, limits);
    }

    pub fn get_limit(&self, stage: StageKind) -> Option<StageLimits> {
        self.limits.get(&stage).copied()
    }
}

/// Runtime accounting ledger verifying queue bounds (count AND bytes)
/// and tracking high-water marks for every stage (Plan §11.2).
#[derive(Debug)]
pub struct PipelineQueueLedger {
    policy: PipelineQueuePolicy,
    usage: HashMap<StageKind, StageUsage>,
    driver_owned_surfaces: HashSet<u64>,
    reference_held_frames: HashMap<u64, usize>,
}

impl PipelineQueueLedger {
    pub fn new(policy: PipelineQueuePolicy) -> Self {
        Self {
            policy,
            usage: HashMap::new(),
            driver_owned_surfaces: HashSet::new(),
            reference_held_frames: HashMap::new(),
        }
    }

    /// Checks whether admitting `bytes` to `stage` would exceed its count or byte limits.
    pub fn check_admission(&self, stage: StageKind, bytes: usize) -> Result<(), PipelineError> {
        let limits = self.policy.get_limit(stage).unwrap_or(StageLimits {
            max_count: usize::MAX,
            max_bytes: usize::MAX,
        });
        let current = self.usage.get(&stage).copied().unwrap_or_default();

        let new_count = current.current_count.saturating_add(1);
        if new_count > limits.max_count {
            return Err(PipelineError::CountLimitExceeded {
                stage,
                current: new_count,
                limit: limits.max_count,
            });
        }

        let new_bytes = current.current_bytes.saturating_add(bytes);
        if new_bytes > limits.max_bytes {
            return Err(PipelineError::ByteLimitExceeded {
                stage,
                current: new_bytes,
                limit: limits.max_bytes,
            });
        }

        Ok(())
    }

    /// Admits an item to the specified stage, updating usage and high-water marks.
    pub fn admit(&mut self, stage: StageKind, bytes: usize) -> Result<(), PipelineError> {
        self.check_admission(stage, bytes)?;
        let usage = self.usage.entry(stage).or_default();
        usage.current_count = usage.current_count.saturating_add(1);
        usage.current_bytes = usage.current_bytes.saturating_add(bytes);
        usage.high_water_count = usage.high_water_count.max(usage.current_count);
        usage.high_water_bytes = usage.high_water_bytes.max(usage.current_bytes);
        Ok(())
    }

    /// Releases an item from the specified stage.
    pub fn release(&mut self, stage: StageKind, bytes: usize) {
        if let Some(usage) = self.usage.get_mut(&stage) {
            usage.current_count = usage.current_count.saturating_sub(1);
            usage.current_bytes = usage.current_bytes.saturating_sub(bytes);
        }
    }

    /// Replaces an existing item in a stage (e.g. `CaptureAdmission` or `Presentation`),
    /// keeping the count constant while adjusting bytes.
    pub fn replace(
        &mut self,
        stage: StageKind,
        old_bytes: usize,
        new_bytes: usize,
    ) -> Result<(), PipelineError> {
        let limits = self.policy.get_limit(stage).unwrap_or(StageLimits {
            max_count: usize::MAX,
            max_bytes: usize::MAX,
        });
        let usage = self.usage.entry(stage).or_default();

        let intermediate_bytes = usage.current_bytes.saturating_sub(old_bytes);
        let final_bytes = intermediate_bytes.saturating_add(new_bytes);

        if final_bytes > limits.max_bytes {
            return Err(PipelineError::ByteLimitExceeded {
                stage,
                current: final_bytes,
                limit: limits.max_bytes,
            });
        }

        if usage.current_count == 0 {
            usage.current_count = 1;
        }
        usage.current_bytes = final_bytes;
        usage.high_water_count = usage.high_water_count.max(usage.current_count);
        usage.high_water_bytes = usage.high_water_bytes.max(usage.current_bytes);
        Ok(())
    }

    /// Registers a surface as owned by the GPU/driver (cannot be freed early).
    pub fn mark_driver_owned(&mut self, surface_id: u64) {
        self.driver_owned_surfaces.insert(surface_id);
    }

    /// Releases driver ownership of a surface once the GPU callback confirms release.
    pub fn release_driver_owned(&mut self, surface_id: u64) {
        self.driver_owned_surfaces.remove(&surface_id);
    }

    /// Attempts to free a surface. If the driver still owns it, typed refusal is returned (Plan §11.2).
    pub fn try_free_surface(&mut self, surface_id: u64) -> Result<(), PipelineError> {
        if self.driver_owned_surfaces.contains(&surface_id) {
            return Err(PipelineError::DriverSurfaceOwnershipViolation { surface_id });
        }
        Ok(())
    }

    /// Implements "skip presentation" separate from "discard decode/reference state" (Plan §11.2).
    /// Releases the display surface while keeping the picture in the reference pool.
    pub fn skip_presentation_retain_reference(
        &mut self,
        frame_id: u64,
        display_bytes: usize,
        ref_bytes: usize,
    ) {
        self.release(StageKind::Presentation, display_bytes);
        self.reference_held_frames.insert(frame_id, ref_bytes);
    }

    /// Releases reference state once future frames no longer depend on this picture.
    pub fn release_reference(&mut self, frame_id: u64) {
        if let Some(bytes) = self.reference_held_frames.remove(&frame_id) {
            self.release(StageKind::DecoderSubmission, bytes);
        }
    }

    /// Returns the current usage of a stage.
    pub fn usage(&self, stage: StageKind) -> StageUsage {
        self.usage.get(&stage).copied().unwrap_or_default()
    }
}

/// A dirty rectangle within a captured display surface (Plan §11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl DamageRect {
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Validates that this rectangle fits within the given surface dimensions.
    pub fn validate_within(
        &self,
        surface_width: u32,
        surface_height: u32,
    ) -> Result<(), PipelineError> {
        let max_x = self
            .x
            .checked_add(self.width)
            .ok_or(PipelineError::OutOfBoundsDamage {
                x: self.x,
                y: self.y,
                width: self.width,
                height: self.height,
                surface_width,
                surface_height,
            })?;
        let max_y = self
            .y
            .checked_add(self.height)
            .ok_or(PipelineError::OutOfBoundsDamage {
                x: self.x,
                y: self.y,
                width: self.width,
                height: self.height,
                surface_width,
                surface_height,
            })?;

        if max_x > surface_width || max_y > surface_height {
            return Err(PipelineError::OutOfBoundsDamage {
                x: self.x,
                y: self.y,
                width: self.width,
                height: self.height,
                surface_width,
                surface_height,
            });
        }

        Ok(())
    }

    /// Computes the pixel area of this rectangle.
    pub const fn area(&self) -> u64 {
        (self.width as u64) * (self.height as u64)
    }
}

/// A bounded collection of damage rectangles (up to 64 per picture).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DamageRegion {
    rects: Vec<DamageRect>,
}

impl DamageRegion {
    pub const MAX_RECTS: usize = 64;

    pub fn new() -> Self {
        Self { rects: Vec::new() }
    }

    pub fn from_rects(rects: Vec<DamageRect>) -> Self {
        let mut r = Self::new();
        for rect in rects.into_iter().take(Self::MAX_RECTS) {
            r.rects.push(rect);
        }
        r
    }

    pub fn add_rect(&mut self, rect: DamageRect) -> bool {
        if self.rects.len() < Self::MAX_RECTS {
            self.rects.push(rect);
            true
        } else {
            false
        }
    }

    pub fn rects(&self) -> &[DamageRect] {
        &self.rects
    }

    pub fn is_empty(&self) -> bool {
        self.rects.is_empty()
    }

    /// Computes the minimal bounding box enclosing all damage rectangles.
    pub fn bounding_box(&self) -> Option<DamageRect> {
        if self.rects.is_empty() {
            return None;
        }
        let mut min_x = u32::MAX;
        let mut min_y = u32::MAX;
        let mut max_x = 0;
        let mut max_y = 0;

        for r in &self.rects {
            min_x = min_x.min(r.x);
            min_y = min_y.min(r.y);
            max_x = max_x.max(r.x.saturating_add(r.width));
            max_y = max_y.max(r.y.saturating_add(r.height));
        }

        Some(DamageRect {
            x: min_x,
            y: min_y,
            width: max_x.saturating_sub(min_x),
            height: max_y.saturating_sub(min_y),
        })
    }

    /// Total sum of areas of all damage rectangles.
    pub fn total_area(&self) -> u64 {
        self.rects.iter().map(DamageRect::area).sum()
    }
}

/// Reconstructs a full valid surface from damage rects before full-frame encoding (Plan §11.4).
/// Prevents encoding uninitialized memory when only partial dirty rectangles are supplied.
#[derive(Debug)]
pub struct DamageSurfaceReconstructor {
    width: u32,
    height: u32,
    surface: Vec<u8>,
    fully_initialized: bool,
}

impl DamageSurfaceReconstructor {
    pub fn new(width: u32, height: u32) -> Self {
        let size = (width as usize)
            .checked_mul(height as usize)
            .and_then(|p| p.checked_mul(4))
            .unwrap_or(0);
        Self {
            width,
            height,
            surface: vec![0u8; size],
            fully_initialized: false,
        }
    }

    /// Clears the surface on resize, format change, or device loss (Plan §11.4).
    pub fn clear_and_resize(&mut self, new_width: u32, new_height: u32) {
        self.width = new_width;
        self.height = new_height;
        let size = (new_width as usize)
            .checked_mul(new_height as usize)
            .and_then(|p| p.checked_mul(4))
            .unwrap_or(0);
        self.surface = vec![0u8; size];
        self.fully_initialized = false;
    }

    /// Applies a full frame, marking the surface fully initialized.
    pub fn apply_full_frame(&mut self, data: &[u8]) -> Result<(), PipelineError> {
        if data.len() != self.surface.len() {
            return Err(PipelineError::OutOfBoundsDamage {
                x: 0,
                y: 0,
                width: self.width,
                height: self.height,
                surface_width: self.width,
                surface_height: self.height,
            });
        }
        self.surface.copy_from_slice(data);
        self.fully_initialized = true;
        Ok(())
    }

    /// Blits a partial damage rectangle onto the reconstructed surface.
    pub fn apply_damage_rect(
        &mut self,
        rect: DamageRect,
        dirty_pixels: &[u8],
    ) -> Result<(), PipelineError> {
        rect.validate_within(self.width, self.height)?;

        let expected_bytes = (rect.width as usize)
            .checked_mul(rect.height as usize)
            .and_then(|p| p.checked_mul(4))
            .ok_or(PipelineError::OutOfBoundsDamage {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                surface_width: self.width,
                surface_height: self.height,
            })?;

        if dirty_pixels.len() != expected_bytes {
            return Err(PipelineError::OutOfBoundsDamage {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                surface_width: self.width,
                surface_height: self.height,
            });
        }

        let stride = (self.width as usize) * 4;
        let rect_stride = (rect.width as usize) * 4;

        for row in 0..rect.height as usize {
            let src_start = row * rect_stride;
            let src_end = src_start + rect_stride;
            let src_row = &dirty_pixels[src_start..src_end];

            let dst_y = (rect.y as usize) + row;
            let dst_x_bytes = (rect.x as usize) * 4;
            let dst_start = dst_y * stride + dst_x_bytes;
            let dst_end = dst_start + rect_stride;

            self.surface[dst_start..dst_end].copy_from_slice(src_row);
        }

        // If this rect covers the entire surface, it becomes fully initialized
        if rect.x == 0 && rect.y == 0 && rect.width == self.width && rect.height == self.height {
            self.fully_initialized = true;
        }

        Ok(())
    }

    /// Verifies that the surface is completely valid and initialized before full-frame encoding.
    pub fn verify_valid_for_encode(&self) -> Result<(), PipelineError> {
        if !self.fully_initialized {
            return Err(PipelineError::SurfaceNotInitialized);
        }
        Ok(())
    }

    pub fn surface_bytes(&self) -> &[u8] {
        &self.surface
    }

    pub fn is_fully_initialized(&self) -> bool {
        self.fully_initialized
    }
}

/// Rendering owner of the cursor (Plan §11.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorRenderingOwner {
    /// Host composites cursor directly into pixels; viewer must not draw a local cursor.
    HostComposited,
    /// Client renders local cursor overlay from reliable shape and replaceable position.
    ClientRendered,
}

/// Effective resolved cursor state for client presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveCursor {
    pub shape_id: u32,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub locked: bool,
    pub is_fallback_shape: bool,
}

/// Bounded cache of reliable cursor shapes and safe fallback logic (Plan §11.4).
#[derive(Debug)]
pub struct CursorPipelineTracker {
    rendering_owner: CursorRenderingOwner,
    geometry: DisplayGeometryGeneration,
    latest_sequence: u64,
    shapes: HashMap<u32, Vec<u8>>,
    shape_order: VecDeque<u32>,
    max_shapes: usize,
    pointer_locked: bool,
}

impl CursorPipelineTracker {
    pub const DEFAULT_MAX_SHAPES: usize = 32;
    pub const FALLBACK_SHAPE_ID: u32 = 0;

    pub fn new(rendering_owner: CursorRenderingOwner, geometry: DisplayGeometryGeneration) -> Self {
        let mut tracker = Self {
            rendering_owner,
            geometry,
            latest_sequence: 0,
            shapes: HashMap::new(),
            shape_order: VecDeque::new(),
            max_shapes: Self::DEFAULT_MAX_SHAPES,
            pointer_locked: false,
        };
        // Seed default 1x1 fallback white dot cursor
        let fallback_dot = vec![0xFF, 0xFF, 0xFF, 0xFF];
        tracker.shapes.insert(Self::FALLBACK_SHAPE_ID, fallback_dot);
        tracker
    }

    pub fn set_rendering_owner(&mut self, owner: CursorRenderingOwner) {
        self.rendering_owner = owner;
    }

    pub fn set_pointer_locked(&mut self, locked: bool) {
        self.pointer_locked = locked;
    }

    pub fn set_geometry(&mut self, geometry: DisplayGeometryGeneration) {
        self.geometry = geometry;
    }

    /// Stores a reliable cursor shape received from the host.
    pub fn store_shape(&mut self, shape: &CursorShape<'_>) {
        if self.shapes.len() >= self.max_shapes && !self.shapes.contains_key(&shape.shape_id) {
            // Evict oldest shape that is not the fallback
            while let Some(oldest) = self.shape_order.pop_front() {
                if oldest != Self::FALLBACK_SHAPE_ID {
                    self.shapes.remove(&oldest);
                    break;
                }
            }
        }
        self.shapes.insert(shape.shape_id, shape.rgba.to_vec());
        self.shape_order.push_back(shape.shape_id);
    }

    /// Processes a replaceable cursor position, enforcing monotonicity, geometry fencing,
    /// single rendering owner, and unknown-shape safe fallback.
    pub fn process_position(
        &mut self,
        pos: &CursorPosition,
    ) -> Result<Option<EffectiveCursor>, PipelineError> {
        // Monotonic sequence check (replaceable datagram drops older sequence)
        if pos.sequence <= self.latest_sequence {
            return Err(PipelineError::StaleSequence {
                sequence: pos.sequence,
                latest: self.latest_sequence,
            });
        }
        self.latest_sequence = pos.sequence;

        // Geometry generation fence
        if pos.geometry_generation != self.geometry.as_raw() {
            return Err(PipelineError::MismatchedGeometry {
                expected: self.geometry,
                actual: DisplayGeometryGeneration::from_raw(pos.geometry_generation),
            });
        }

        // If host composites cursor or pointer is locked / hidden, do not render a local double cursor
        if self.rendering_owner == CursorRenderingOwner::HostComposited
            || !pos.is_visible()
            || self.pointer_locked
            || pos.is_locked()
        {
            return Ok(Some(EffectiveCursor {
                shape_id: pos.shape_id,
                x: pos.x,
                y: pos.y,
                visible: false,
                locked: self.pointer_locked || pos.is_locked(),
                is_fallback_shape: false,
            }));
        }

        // Shape resolution with safe fallback if shape has not arrived yet
        let (resolved_id, is_fallback) = if self.shapes.contains_key(&pos.shape_id) {
            (pos.shape_id, false)
        } else {
            (Self::FALLBACK_SHAPE_ID, true)
        };

        Ok(Some(EffectiveCursor {
            shape_id: resolved_id,
            x: pos.x,
            y: pos.y,
            visible: true,
            locked: false,
            is_fallback_shape: is_fallback,
        }))
    }
}

/// Tracks the two distinct ages of useful state (Plan §11.3):
/// 1. Age of last pixel update (when desktop pixels actually changed).
/// 2. Age of last trustworthy source observation (serviced capture op, OS-qualified damage/unchanged evidence).
#[derive(Debug, Clone)]
pub struct TwoAgesTracker {
    last_pixel_update_us: u64,
    last_source_observation_us: u64,
    last_source_scope: SourceObservation,
    last_heartbeat_us: u64,
    max_source_staleness_us: u64,
}

impl TwoAgesTracker {
    pub const DEFAULT_MAX_STALENESS_US: u64 = 1_500_000; // 1.5 seconds

    pub fn new(initial_time_us: u64) -> Self {
        Self {
            last_pixel_update_us: initial_time_us,
            last_source_observation_us: initial_time_us,
            last_source_scope: SourceObservation::Unknown,
            last_heartbeat_us: initial_time_us,
            max_source_staleness_us: Self::DEFAULT_MAX_STALENESS_US,
        }
    }

    pub fn set_max_staleness_us(&mut self, max_staleness_us: u64) {
        self.max_source_staleness_us = max_staleness_us;
    }

    /// Records that desktop pixels changed and were encoded.
    pub fn record_pixel_update(&mut self, timestamp_us: u64) {
        self.last_pixel_update_us = timestamp_us;
        self.last_source_observation_us = timestamp_us;
        self.last_source_scope = SourceObservation::Captured;
    }

    /// Records a trustworthy source observation that the source remains unchanged
    /// (e.g. from OS damage listener or bounded verification probe) without updating pixel age.
    pub fn record_source_observation(&mut self, timestamp_us: u64, scope: SourceObservation) {
        self.last_source_observation_us = timestamp_us;
        self.last_source_scope = scope;
    }

    /// Records a transport network heartbeat. Note: a heartbeat alone never refreshes source age!
    pub fn record_heartbeat(&mut self, timestamp_us: u64) {
        self.last_heartbeat_us = timestamp_us;
    }

    pub fn pixel_age_us(&self, now_us: u64) -> u64 {
        now_us.saturating_sub(self.last_pixel_update_us)
    }

    pub fn source_observation_age_us(&self, now_us: u64) -> u64 {
        now_us.saturating_sub(self.last_source_observation_us)
    }

    pub fn heartbeat_age_us(&self, now_us: u64) -> u64 {
        now_us.saturating_sub(self.last_heartbeat_us)
    }

    pub fn last_source_scope(&self) -> SourceObservation {
        self.last_source_scope
    }

    /// True if capture has stalled and no trustworthy observation has arrived,
    /// even if network heartbeats remain completely healthy (Plan §11.3).
    pub fn is_view_stale(&self, now_us: u64) -> bool {
        self.source_observation_age_us(now_us) > self.max_source_staleness_us
    }
}

/// Action to take on the capture / encode pipeline for a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleAction {
    /// Normal damage occurred: encode and transmit full picture.
    EncodeStandardFrame,
    /// Desktop settled: encode one high-quality "settle-to-sharp" refinement picture (Plan §14.1).
    EncodeSettleToSharpRefinement,
    /// Screen is stationary: stop continuous video transmission (near-zero encode work).
    SkipVideoTransmission,
    /// Verification interval reached during idle: emit a bounded source verification probe.
    EmitSourceVerificationProbe,
}

/// Current state of the desktop activity detector (Plan §13.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleState {
    Active,
    Settling { stationary_since_us: u64 },
    SettleToSharpPending,
    Idle { idle_since_us: u64 },
    CaptureStalled,
}

/// Deterministic stationary-screen detector and idle controller (Plan §13.4).
/// Distinguishes an idle desktop from a hung capture pipeline.
#[derive(Debug)]
pub struct IdleController {
    state: IdleState,
    settle_threshold_us: u64,
    verification_interval_us: u64,
    last_verification_us: u64,
    two_ages: TwoAgesTracker,
}

impl IdleController {
    pub const DEFAULT_SETTLE_THRESHOLD_US: u64 = 400_000; // 400 ms
    pub const DEFAULT_VERIFICATION_INTERVAL_US: u64 = 1_000_000; // 1 second

    pub fn new(initial_time_us: u64) -> Self {
        Self {
            state: IdleState::Active,
            settle_threshold_us: Self::DEFAULT_SETTLE_THRESHOLD_US,
            verification_interval_us: Self::DEFAULT_VERIFICATION_INTERVAL_US,
            last_verification_us: initial_time_us,
            two_ages: TwoAgesTracker::new(initial_time_us),
        }
    }

    pub fn two_ages(&self) -> &TwoAgesTracker {
        &self.two_ages
    }

    pub fn two_ages_mut(&mut self) -> &mut TwoAgesTracker {
        &mut self.two_ages
    }

    pub fn state(&self) -> IdleState {
        self.state
    }

    /// Evaluates incoming frame evidence.
    /// `has_damage`: true if screen contents actually changed.
    pub fn on_frame_event(&mut self, has_damage: bool, now_us: u64) -> IdleAction {
        if has_damage {
            self.state = IdleState::Active;
            self.two_ages.record_pixel_update(now_us);
            return IdleAction::EncodeStandardFrame;
        }

        // Screen is stationary
        match self.state {
            IdleState::Active => {
                self.state = IdleState::Settling {
                    stationary_since_us: now_us,
                };
                IdleAction::SkipVideoTransmission
            }
            IdleState::Settling {
                stationary_since_us,
            } => {
                if now_us.saturating_sub(stationary_since_us) >= self.settle_threshold_us {
                    self.state = IdleState::SettleToSharpPending;
                    IdleAction::EncodeSettleToSharpRefinement
                } else {
                    IdleAction::SkipVideoTransmission
                }
            }
            IdleState::SettleToSharpPending => {
                self.state = IdleState::Idle {
                    idle_since_us: now_us,
                };
                self.last_verification_us = now_us;
                IdleAction::SkipVideoTransmission
            }
            IdleState::Idle { .. } => {
                // Check if it's time for periodic source verification
                if now_us.saturating_sub(self.last_verification_us) >= self.verification_interval_us
                {
                    self.last_verification_us = now_us;
                    IdleAction::EmitSourceVerificationProbe
                } else {
                    IdleAction::SkipVideoTransmission
                }
            }
            IdleState::CaptureStalled => IdleAction::SkipVideoTransmission,
        }
    }

    /// Acknowledges successful source verification during idle.
    pub fn on_source_verification(&mut self, now_us: u64) {
        self.last_verification_us = now_us;
        self.two_ages
            .record_source_observation(now_us, SourceObservation::QualifiedUnchanged);
    }
}
