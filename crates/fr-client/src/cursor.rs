#![forbid(unsafe_code)]
//! Client-side hardware cursor prediction and damage synchronization (plan §11.4, bead fr-8xi).
//!
//! Provides immediate local pointer feedback without round-trip latency:
//! - Predicts pointer position immediately while retaining the distinction between predicted
//!   local coordinates and confirmed host behavior.
//! - Clamps pointer coordinates to active display bounds and updates on geometry generation advance.
//! - Computes exact cursor damage union rectangles to invalidate only dirty regions on motion.
//! - Suppresses local cursor rendering when the host video stream is host-composited (`SHAPE_FLAG_HOST_COMPOSITED`)
//!   to prevent double-cursor artifacts.
//! - Measures coordinate drift between predicted local motion and host-acknowledged state for diagnostics.

use fr_core::{
    ids::DisplayGeometryGeneration,
    input::{DesktopPoint, InputBounds},
};
use fr_wire::cursor::{
    CursorPosition, CursorShape, POSITION_FLAG_LOCKED, POSITION_FLAG_VISIBLE,
    SHAPE_FLAG_HOST_COMPOSITED, SHAPE_FLAG_VISIBLE,
};

/// An axis-aligned dirty damage rectangle on the client display surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorDamageRect {
    /// Leftmost coordinate in desktop pixels.
    pub x: i32,
    /// Topmost coordinate in desktop pixels.
    pub y: i32,
    /// Width in pixels (>= 1).
    pub width: u32,
    /// Height in pixels (>= 1).
    pub height: u32,
}

impl CursorDamageRect {
    /// Create a new damage rectangle with non-zero dimensions.
    #[must_use]
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Option<Self> {
        if width == 0 || height == 0 {
            return None;
        }
        Some(Self {
            x,
            y,
            width,
            height,
        })
    }

    /// Compute the minimal bounding rectangle covering both damage rectangles.
    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let min_x = self.x.min(other.x);
        let min_y = self.y.min(other.y);
        let max_x = (i64::from(self.x) + i64::from(self.width))
            .max(i64::from(other.x) + i64::from(other.width));
        let max_y = (i64::from(self.y) + i64::from(self.height))
            .max(i64::from(other.y) + i64::from(other.height));

        let width = u32::try_from(max_x - i64::from(min_x)).unwrap_or(u32::MAX);
        let height = u32::try_from(max_y - i64::from(min_y)).unwrap_or(u32::MAX);

        Self {
            x: min_x,
            y: min_y,
            width: width.max(1),
            height: height.max(1),
        }
    }
}

/// Metadata and RGBA bitmap storage for the active cursor shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorShapeMetadata {
    /// Opaque shape identifier assigned by host.
    pub shape_id: u32,
    /// Width in pixels (<= 256).
    pub width: u16,
    /// Height in pixels (<= 256).
    pub height: u16,
    /// Horizontal hotspot offset from left edge.
    pub hotspot_x: u16,
    /// Vertical hotspot offset from top edge.
    pub hotspot_y: u16,
    /// Scale factor fixed-point (scale * 1000).
    pub scale_1000: u16,
    /// Whether the cursor is marked visible.
    pub visible: bool,
    /// Whether the cursor is already composited on the host stream.
    pub host_composited: bool,
    /// RGBA8 pixel bytes.
    pub rgba: Vec<u8>,
}

/// Client-side hardware cursor prediction and synchronization controller.
pub struct PredictedCursor {
    /// Currently predicted local cursor coordinate (immediate feedback).
    predicted: DesktopPoint,
    /// Last confirmed position from host `CursorPosition` record.
    confirmed: Option<DesktopPoint>,
    /// Last confirmed position sequence number.
    confirmed_sequence: u64,
    /// Active cursor shape metadata.
    shape: Option<CursorShapeMetadata>,
    /// Whether pointer lock is currently active.
    pointer_locked: bool,
    /// Active display boundary limits for coordinate clamping.
    bounds: Option<InputBounds>,
    /// Generation of the display geometry the cursor is synchronized against.
    geometry_generation: DisplayGeometryGeneration,
}

impl PredictedCursor {
    /// Initialize a cursor predictor with initial geometry generation and optional display bounds.
    #[must_use]
    pub fn new(
        geometry_generation: DisplayGeometryGeneration,
        bounds: Option<InputBounds>,
    ) -> Self {
        let origin = bounds.map_or(DesktopPoint { x: 0, y: 0 }, InputBounds::origin);
        Self {
            predicted: origin,
            confirmed: None,
            confirmed_sequence: 0,
            shape: None,
            pointer_locked: false,
            bounds,
            geometry_generation,
        }
    }

    /// Update display geometry and boundary limits. Re-clamps predicted position if bounds changed.
    pub fn update_bounds(
        &mut self,
        generation: DisplayGeometryGeneration,
        bounds: Option<InputBounds>,
    ) -> Option<CursorDamageRect> {
        self.geometry_generation = generation;
        self.bounds = bounds;
        let old_pos = self.predicted;
        let clamped = self.clamp_point(old_pos);
        if clamped == old_pos {
            None
        } else {
            self.predicted = clamped;
            self.compute_damage(old_pos, clamped)
        }
    }

    /// Handle immediate local pointer motion event from client windowing system.
    ///
    /// Clamps coordinates to display bounds and returns a damage rectangle covering
    /// both previous and new cursor positions if the cursor moved.
    pub fn on_local_move(&mut self, target: DesktopPoint) -> Option<CursorDamageRect> {
        if self.pointer_locked {
            // Under pointer lock, coordinates remain fixed; relative deltas are dispatched separately.
            return None;
        }

        let clamped = self.clamp_point(target);
        let old_pos = self.predicted;
        if clamped == old_pos {
            return None;
        }

        self.predicted = clamped;
        self.compute_damage(old_pos, clamped)
    }

    /// Ingest a confirmed `CursorPosition` record received from the host.
    pub fn handle_position_record(&mut self, pos: &CursorPosition) -> Option<CursorDamageRect> {
        if pos.sequence < self.confirmed_sequence {
            // Reject out-of-order or stale position record.
            return None;
        }

        self.confirmed_sequence = pos.sequence;
        let host_point = DesktopPoint { x: pos.x, y: pos.y };
        self.confirmed = Some(host_point);
        self.pointer_locked = (pos.flags & POSITION_FLAG_LOCKED) != 0;

        let visible = (pos.flags & POSITION_FLAG_VISIBLE) != 0;
        if let Some(shape) = &mut self.shape {
            shape.visible = visible;
        }

        None
    }

    /// Ingest a reliable `CursorShape` record received from the host on `MediaConfig`.
    pub fn handle_shape_record(&mut self, shape: &CursorShape<'_>) -> Option<CursorDamageRect> {
        let old_rect = self.current_bounding_rect();

        let visible = (shape.flags & SHAPE_FLAG_VISIBLE) != 0;
        let host_composited = (shape.flags & SHAPE_FLAG_HOST_COMPOSITED) != 0;

        self.shape = Some(CursorShapeMetadata {
            shape_id: shape.shape_id,
            width: shape.width,
            height: shape.height,
            hotspot_x: shape.hotspot_x,
            hotspot_y: shape.hotspot_y,
            scale_1000: shape.scale_1000,
            visible,
            host_composited,
            rgba: shape.rgba.to_vec(),
        });

        let new_rect = self.current_bounding_rect();

        match (old_rect, new_rect) {
            (Some(a), Some(b)) => Some(a.union(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        }
    }

    /// Whether the client renderer should draw a local hardware cursor.
    ///
    /// Strictly returns `false` when:
    /// - Cursor is host-composited (`SHAPE_FLAG_HOST_COMPOSITED` is set).
    /// - Pointer is locked (`POSITION_FLAG_LOCKED`).
    /// - Cursor is marked invisible (`visible == false`).
    #[must_use]
    pub fn should_render_local(&self) -> bool {
        if self.pointer_locked {
            return false;
        }
        if let Some(shape) = &self.shape {
            shape.visible && !shape.host_composited
        } else {
            // Default to local rendering when no host shape received yet
            true
        }
    }

    /// Currently predicted desktop coordinates.
    #[must_use]
    pub const fn predicted_position(&self) -> DesktopPoint {
        self.predicted
    }

    /// Last confirmed host position, if received.
    #[must_use]
    pub const fn confirmed_position(&self) -> Option<DesktopPoint> {
        self.confirmed
    }

    /// Active cursor shape metadata, if received.
    #[must_use]
    pub fn shape(&self) -> Option<&CursorShapeMetadata> {
        self.shape.as_ref()
    }

    /// Calculate the squared Euclidean distance between predicted and confirmed positions.
    /// Used for drift tracking and connection diagnostics.
    #[must_use]
    pub fn drift_distance_squared(&self) -> Option<u64> {
        self.confirmed.map(|conf| {
            let dx = i64::from(self.predicted.x) - i64::from(conf.x);
            let dy = i64::from(self.predicted.y) - i64::from(conf.y);
            u64::try_from(dx * dx + dy * dy).unwrap_or(u64::MAX)
        })
    }

    /// Compute bounding rectangle of the cursor at the specified point.
    fn rect_at(&self, pos: DesktopPoint) -> Option<CursorDamageRect> {
        let (width, height, hot_x, hot_y) = if let Some(shape) = &self.shape {
            (
                u32::from(shape.width),
                u32::from(shape.height),
                i32::from(shape.hotspot_x),
                i32::from(shape.hotspot_y),
            )
        } else {
            // Standard default arrow cursor fallback dimensions (16x16, hotspot at 0,0)
            (16, 16, 0, 0)
        };

        let x = pos.x.checked_sub(hot_x)?;
        let y = pos.y.checked_sub(hot_y)?;
        CursorDamageRect::new(x, y, width, height)
    }

    /// Bounding rectangle at the current predicted position.
    #[must_use]
    pub fn current_bounding_rect(&self) -> Option<CursorDamageRect> {
        self.rect_at(self.predicted)
    }

    /// Compute damage union between old and new positions.
    fn compute_damage(
        &self,
        old_pos: DesktopPoint,
        new_pos: DesktopPoint,
    ) -> Option<CursorDamageRect> {
        let r1 = self.rect_at(old_pos)?;
        let r2 = self.rect_at(new_pos)?;
        Some(r1.union(r2))
    }

    /// Clamp a point strictly within active bounds.
    fn clamp_point(&self, point: DesktopPoint) -> DesktopPoint {
        if let Some(bounds) = self.bounds {
            let min_x = bounds.origin().x;
            let min_y = bounds.origin().y;
            let width_offset = i32::try_from(bounds.width().saturating_sub(1)).unwrap_or(i32::MAX);
            let height_offset =
                i32::try_from(bounds.height().saturating_sub(1)).unwrap_or(i32::MAX);
            let max_x = min_x.saturating_add(width_offset);
            let max_y = min_y.saturating_add(height_offset);

            DesktopPoint {
                x: point.x.clamp(min_x, max_x),
                y: point.y.clamp(min_y, max_y),
            }
        } else {
            point
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_damage_rect_union() {
        let r1 = CursorDamageRect::new(10, 20, 30, 40).unwrap();
        let r2 = CursorDamageRect::new(25, 35, 30, 40).unwrap();
        let u = r1.union(r2);

        assert_eq!(u.x, 10);
        assert_eq!(u.y, 20);
        assert_eq!(u.width, 45); // from 10 to 55
        assert_eq!(u.height, 55); // from 20 to 75
    }

    #[test]
    fn cursor_boundary_clamping_to_display() {
        let geometry_gen = DisplayGeometryGeneration::INITIAL;
        let bounds = InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).unwrap();
        let mut cursor = PredictedCursor::new(geometry_gen, Some(bounds));

        // Moving within bounds
        let damage = cursor.on_local_move(DesktopPoint { x: 100, y: 200 });
        assert!(damage.is_some());
        assert_eq!(cursor.predicted_position(), DesktopPoint { x: 100, y: 200 });

        // Moving outside right/bottom boundary clamps to 1919, 1079
        cursor.on_local_move(DesktopPoint { x: 2500, y: 3000 });
        assert_eq!(
            cursor.predicted_position(),
            DesktopPoint { x: 1919, y: 1079 }
        );

        // Moving outside left/top boundary clamps to 0, 0
        cursor.on_local_move(DesktopPoint { x: -50, y: -100 });
        assert_eq!(cursor.predicted_position(), DesktopPoint { x: 0, y: 0 });
    }

    #[test]
    fn host_composited_cursor_suppresses_local_rendering() {
        let geometry_gen = DisplayGeometryGeneration::INITIAL;
        let mut cursor = PredictedCursor::new(geometry_gen, None);

        assert!(cursor.should_render_local());

        // Ingest a shape marked host-composited
        let shape = CursorShape {
            shape_id: 1,
            width: 32,
            height: 32,
            hotspot_x: 0,
            hotspot_y: 0,
            scale_1000: 1000,
            flags: SHAPE_FLAG_VISIBLE | SHAPE_FLAG_HOST_COMPOSITED,
            rgba: &[0u8; 32 * 32 * 4],
        };
        cursor.handle_shape_record(&shape);

        // Local rendering must be suppressed to avoid double cursor
        assert!(!cursor.should_render_local());

        // Ingest a regular visible shape without host-composited flag
        let regular_shape = CursorShape {
            flags: SHAPE_FLAG_VISIBLE,
            ..shape
        };
        cursor.handle_shape_record(&regular_shape);
        assert!(cursor.should_render_local());
    }

    #[test]
    fn pointer_lock_suppresses_local_motion_and_rendering() {
        let geometry_gen = DisplayGeometryGeneration::INITIAL;
        let mut cursor = PredictedCursor::new(geometry_gen, None);

        // Position record with LOCKED flag
        let pos = CursorPosition {
            shape_id: 1,
            sequence: 1,
            x: 500,
            y: 500,
            geometry_generation: 1,
            flags: POSITION_FLAG_VISIBLE | POSITION_FLAG_LOCKED,
        };
        cursor.handle_position_record(&pos);

        // Under pointer lock, local rendering is suppressed
        assert!(!cursor.should_render_local());

        // Moving does not alter predicted position under lock
        let damage = cursor.on_local_move(DesktopPoint { x: 600, y: 600 });
        assert!(damage.is_none());
        assert_eq!(cursor.predicted_position(), DesktopPoint { x: 0, y: 0 });
    }

    #[test]
    fn drift_tracking_measures_distance_accurately() {
        let geometry_gen = DisplayGeometryGeneration::INITIAL;
        let mut cursor = PredictedCursor::new(geometry_gen, None);

        cursor.on_local_move(DesktopPoint { x: 103, y: 204 });

        let pos = CursorPosition {
            shape_id: 1,
            sequence: 1,
            x: 100,
            y: 200,
            geometry_generation: 1,
            flags: POSITION_FLAG_VISIBLE,
        };
        cursor.handle_position_record(&pos);

        // dx = 3, dy = 4 -> 3^2 + 4^2 = 25
        assert_eq!(cursor.drift_distance_squared(), Some(25));
    }
}
