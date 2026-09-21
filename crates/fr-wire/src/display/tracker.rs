//! Host display topology tracking, multi-display stream management,
//! and single-controller explicit target routing (Plan §15.5, §24.2).
//!
//! # Invariants:
//! - Stable local identity within OS guarantees: 128-bit handles are cryptographically
//!   and monotonically distinct across hotplugs and connector reuses.
//! - Any hotplug/reconfiguration (add, remove, resize, rescale, rotation) creates
//!   a new `DisplayGeometryGeneration` BEFORE coordinate input is accepted.
//! - Missing or ambiguous mappings refuse rather than guess (inter-monitor gaps,
//!   out-of-bounds, overlapping displays).
//! - Only selected displays transmitted; unobserved displays suspended.
//! - The single controller's pointer/keyboard target is explicit when viewing several displays.
//! - Fault tolerance: Drag across display removal refuses rather than misdelivers;
//!   click across scale change refuses stale geometry.

use super::{Catalog, Display, DisplayMappingError, MAX_DISPLAYS};
use fr_core::{
    ids::{DisplayGeometryGeneration, OsSessionId},
    input::{DesktopPoint, PointerButton},
    limits::ProtocolLimits,
};

/// Raw OS monitor descriptor passed by platform display enumeration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostMonitorDescriptor {
    pub connector_id: [u8; 32],
    pub connector_id_len: usize,
    pub x: i32,
    pub y: i32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub logical_width: u32,
    pub logical_height: u32,
    pub scale_numerator: u32,
    pub scale_denominator: u32,
    pub rotation: u8,
}

impl HostMonitorDescriptor {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        connector: &str,
        x: i32,
        y: i32,
        pixel_width: u32,
        pixel_height: u32,
        logical_width: u32,
        logical_height: u32,
        scale_numerator: u32,
        scale_denominator: u32,
        rotation: u8,
    ) -> Self {
        let bytes = connector.as_bytes();
        let len = bytes.len().min(32);
        let mut id = [0u8; 32];
        id[..len].copy_from_slice(&bytes[..len]);
        Self {
            connector_id: id,
            connector_id_len: len,
            x,
            y,
            pixel_width,
            pixel_height,
            logical_width,
            logical_height,
            scale_numerator,
            scale_denominator,
            rotation,
        }
    }

    pub fn connector_name(&self) -> &str {
        std::str::from_utf8(&self.connector_id[..self.connector_id_len]).unwrap_or("<invalid-utf8>")
    }

    pub fn matches_topology(&self, other: &Self) -> bool {
        self.connector_id_len == other.connector_id_len
            && self.connector_id[..self.connector_id_len]
                == other.connector_id[..other.connector_id_len]
            && self.x == other.x
            && self.y == other.y
            && self.pixel_width == other.pixel_width
            && self.pixel_height == other.pixel_height
            && self.logical_width == other.logical_width
            && self.logical_height == other.logical_height
            && self.scale_numerator == other.scale_numerator
            && self.scale_denominator == other.scale_denominator
            && self.rotation == other.rotation
    }
}

/// Result of a topology update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopologyUpdate {
    pub catalog: Catalog,
    pub geometry_changed: bool,
    pub added_count: usize,
    pub removed_count: usize,
}

/// Tracks host display catalog across hotplug and reconfiguration events.
/// Handles connector and numeric ID reuse by issuing fresh unique 128-bit handles
/// and monotonically advancing `DisplayGeometryGeneration`.
pub struct DisplayCatalogTracker {
    os_session: OsSessionId,
    geometry_generation: DisplayGeometryGeneration,
    revision: u64,
    instance_counter: u64,
    active_catalog: Catalog,
    limits: ProtocolLimits,
    current_monitors: Vec<(HostMonitorDescriptor, u128)>,
}

impl DisplayCatalogTracker {
    pub fn new(os_session: OsSessionId, limits: ProtocolLimits) -> Self {
        let empty_catalog = Catalog::new(1, &[], &limits).expect("empty catalog is always valid");
        Self {
            os_session,
            geometry_generation: DisplayGeometryGeneration::INITIAL,
            revision: 1,
            instance_counter: 1,
            active_catalog: empty_catalog,
            limits,
            current_monitors: Vec::new(),
        }
    }

    pub fn os_session(&self) -> OsSessionId {
        self.os_session
    }

    pub fn geometry_generation(&self) -> DisplayGeometryGeneration {
        self.geometry_generation
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn active_catalog(&self) -> &Catalog {
        &self.active_catalog
    }

    /// Update host topology with a new list of OS monitor descriptors.
    /// If ANY monitor is added, removed, or changed, this:
    /// 1. Advances `DisplayGeometryGeneration` BEFORE new input is accepted.
    /// 2. Generates new 128-bit handles (preventing connector reuse races).
    /// 3. Advances catalog revision and updates active catalog.
    pub fn update_topology(
        &mut self,
        monitors: &[HostMonitorDescriptor],
    ) -> Result<TopologyUpdate, crate::WireError> {
        if monitors.len() > MAX_DISPLAYS {
            return Err(crate::WireError::ResourceLimit);
        }

        // Check if current topology matches exactly
        let unchanged = self.current_monitors.len() == monitors.len()
            && self
                .current_monitors
                .iter()
                .zip(monitors.iter())
                .all(|((curr, _), next)| curr.matches_topology(next));

        if unchanged {
            return Ok(TopologyUpdate {
                catalog: self.active_catalog,
                geometry_changed: false,
                added_count: 0,
                removed_count: 0,
            });
        }

        // Topology changed! Advance geometry generation and revision.
        let next_gen = self
            .geometry_generation
            .next()
            .ok_or(crate::WireError::ResourceLimit)?;
        let next_rev = self
            .revision
            .checked_add(1)
            .ok_or(crate::WireError::ResourceLimit)?;

        let mut displays = Vec::with_capacity(monitors.len());
        let mut new_monitors = Vec::with_capacity(monitors.len());
        let mut added_count = 0;

        for m in monitors {
            // Assign a unique 128-bit handle using the new geometry generation
            // and monotonic instance counter. This guarantees no collision with old handles.
            let handle =
                ((u128::from(next_gen.as_raw())) << 64) | u128::from(self.instance_counter);
            self.instance_counter = self
                .instance_counter
                .checked_add(1)
                .ok_or(crate::WireError::ResourceLimit)?;

            let display = Display {
                handle,
                geometry: next_gen,
                x: m.x,
                y: m.y,
                pixel_width: m.pixel_width,
                pixel_height: m.pixel_height,
                logical_width: m.logical_width,
                logical_height: m.logical_height,
                scale_numerator: m.scale_numerator,
                scale_denominator: m.scale_denominator,
                rotation: m.rotation,
            };
            display.validate(&self.limits)?;
            displays.push(display);
            new_monitors.push((*m, handle));

            if !self
                .current_monitors
                .iter()
                .any(|(curr, _)| curr.matches_topology(m))
            {
                added_count += 1;
            }
        }

        let removed_count = self
            .current_monitors
            .iter()
            .filter(|(curr, _)| !monitors.iter().any(|m| m.matches_topology(curr)))
            .count();

        let new_catalog = Catalog::new(next_rev, &displays, &self.limits)?;
        self.geometry_generation = next_gen;
        self.revision = next_rev;
        self.active_catalog = new_catalog;
        self.current_monitors = new_monitors;

        Ok(TopologyUpdate {
            catalog: new_catalog,
            geometry_changed: true,
            added_count,
            removed_count,
        })
    }

    /// Map a coordinate to an unambiguous display, validating geometry generation first.
    pub fn map_coordinate(
        &self,
        x: i32,
        y: i32,
        geometry: DisplayGeometryGeneration,
    ) -> Result<Display, DisplayMappingError> {
        if geometry != self.geometry_generation {
            return Err(DisplayMappingError::StaleGeometry);
        }
        self.active_catalog.map_pixel_coordinate(x, y)
    }

    /// Validate that a display handle exists in the active catalog and matches geometry.
    pub fn validate_display_handle(
        &self,
        handle: u128,
        geometry: DisplayGeometryGeneration,
    ) -> Result<Display, DisplayMappingError> {
        if geometry != self.geometry_generation {
            return Err(DisplayMappingError::StaleGeometry);
        }
        self.active_catalog
            .find(handle)
            .ok_or(DisplayMappingError::DisplayNotFound)
    }
}

/// State of an individual display media stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayStreamState {
    /// No viewers observing; capture and encoding suspended.
    Suspended,
    /// One or more viewers subscribed; active media transmission.
    Active,
}

/// Track which displays in the catalog are transmitted vs suspended under
/// the shared session budget.
pub struct MultiDisplayStreamManager {
    streams: Vec<DisplayStreamEntry>,
}

#[derive(Debug, Clone)]
struct DisplayStreamEntry {
    handle: u128,
    channel_id: u32,
    state: DisplayStreamState,
    subscribers: usize,
}

impl Default for MultiDisplayStreamManager {
    fn default() -> Self {
        Self::new()
    }
}

impl MultiDisplayStreamManager {
    pub fn new() -> Self {
        Self {
            streams: Vec::new(),
        }
    }

    /// Synchronize stream entries with an updated catalog.
    /// Retires streams for removed displays; sets up suspended streams for new displays.
    pub fn sync_catalog(&mut self, catalog: &Catalog, base_channel: u32) {
        let displays = catalog.displays();
        // Remove streams for displays no longer in catalog
        self.streams
            .retain(|entry| displays.iter().any(|d| d.handle == entry.handle));

        // Add streams for new displays
        for (i, d) in displays.iter().enumerate() {
            if !self.streams.iter().any(|entry| entry.handle == d.handle) {
                self.streams.push(DisplayStreamEntry {
                    handle: d.handle,
                    channel_id: base_channel + u32::try_from(i).unwrap_or(0),
                    state: DisplayStreamState::Suspended,
                    subscribers: 0,
                });
            }
        }
    }

    /// Subscribe a viewer to a display, activating the stream if previously suspended.
    pub fn subscribe(&mut self, handle: u128) -> Result<u32, DisplayMappingError> {
        let entry = self
            .streams
            .iter_mut()
            .find(|e| e.handle == handle)
            .ok_or(DisplayMappingError::DisplayNotFound)?;
        entry.subscribers = entry.subscribers.saturating_add(1);
        entry.state = DisplayStreamState::Active;
        Ok(entry.channel_id)
    }

    /// Unsubscribe a viewer from a display. When 0 subscribers remain, the stream is suspended.
    pub fn unsubscribe(&mut self, handle: u128) -> Result<(), DisplayMappingError> {
        let entry = self
            .streams
            .iter_mut()
            .find(|e| e.handle == handle)
            .ok_or(DisplayMappingError::DisplayNotFound)?;
        entry.subscribers = entry.subscribers.saturating_sub(1);
        if entry.subscribers == 0 {
            entry.state = DisplayStreamState::Suspended;
        }
        Ok(())
    }

    pub fn is_active(&self, handle: u128) -> bool {
        self.streams
            .iter()
            .find(|e| e.handle == handle)
            .is_some_and(|e| e.state == DisplayStreamState::Active)
    }

    pub fn is_suspended(&self, handle: u128) -> bool {
        self.streams
            .iter()
            .find(|e| e.handle == handle)
            .is_none_or(|e| e.state == DisplayStreamState::Suspended)
    }

    pub fn active_stream_count(&self) -> usize {
        self.streams
            .iter()
            .filter(|e| e.state == DisplayStreamState::Active)
            .count()
    }
}

/// Tracks the single controller's explicit target display and anchors drag/click
/// operations to their origin display and geometry.
pub struct DisplayTargetController {
    explicit_target: Option<u128>,
    active_drag: Option<ActiveDragState>,
    active_click: Option<ActiveClickState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveDragState {
    target_display: u128,
    geometry: DisplayGeometryGeneration,
    button: PointerButton,
    current_pos: DesktopPoint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ActiveClickState {
    target_display: u128,
    geometry: DisplayGeometryGeneration,
    button: PointerButton,
    scale_numerator: u32,
    scale_denominator: u32,
}

impl Default for DisplayTargetController {
    fn default() -> Self {
        Self::new()
    }
}

impl DisplayTargetController {
    pub fn new() -> Self {
        Self {
            explicit_target: None,
            active_drag: None,
            active_click: None,
        }
    }

    pub fn explicit_target(&self) -> Option<u128> {
        self.explicit_target
    }

    pub fn set_explicit_target(
        &mut self,
        handle: u128,
        catalog: &Catalog,
    ) -> Result<(), DisplayMappingError> {
        if catalog.find(handle).is_none() {
            return Err(DisplayMappingError::DisplayNotFound);
        }
        self.explicit_target = Some(handle);
        Ok(())
    }

    /// Process pointer motion.
    /// If a drag is active: validates that the dragged display still exists in the catalog
    /// and that geometry generation has not advanced.
    /// If not dragging: maps coordinates to an unambiguous display and updates the target.
    pub fn route_pointer_move(
        &mut self,
        x: i32,
        y: i32,
        geometry: DisplayGeometryGeneration,
        catalog: &Catalog,
    ) -> Result<(u128, DesktopPoint), DisplayMappingError> {
        if let Some(drag) = &mut self.active_drag {
            // Fault test invariant: Remove display during a drag
            if let Some(cat_gen) = catalog.geometry_generation()
                && cat_gen != drag.geometry
            {
                return Err(DisplayMappingError::StaleGeometry);
            }
            if drag.geometry != geometry {
                return Err(DisplayMappingError::StaleGeometry);
            }
            let d = catalog
                .find(drag.target_display)
                .ok_or(DisplayMappingError::DisplayNotFound)?;
            drag.current_pos = DesktopPoint { x, y };
            Ok((d.handle, drag.current_pos))
        } else {
            // Normal pointer move: validate geometry and map coordinates
            if let Some(cat_gen) = catalog.geometry_generation()
                && cat_gen != geometry
            {
                return Err(DisplayMappingError::StaleGeometry);
            }
            let display = catalog.map_pixel_coordinate(x, y)?;
            self.explicit_target = Some(display.handle);
            Ok((display.handle, DesktopPoint { x, y }))
        }
    }

    /// Process pointer press. Anchors drag and click state to the target display,
    /// its geometry generation, and current scale factor.
    pub fn on_pointer_press(
        &mut self,
        button: PointerButton,
        x: i32,
        y: i32,
        geometry: DisplayGeometryGeneration,
        catalog: &Catalog,
    ) -> Result<u128, DisplayMappingError> {
        if let Some(cat_gen) = catalog.geometry_generation()
            && cat_gen != geometry
        {
            return Err(DisplayMappingError::StaleGeometry);
        }
        let display = catalog.map_pixel_coordinate(x, y)?;
        let handle = display.handle;
        self.explicit_target = Some(handle);

        self.active_drag = Some(ActiveDragState {
            target_display: handle,
            geometry,
            button,
            current_pos: DesktopPoint { x, y },
        });

        self.active_click = Some(ActiveClickState {
            target_display: handle,
            geometry,
            button,
            scale_numerator: display.scale_numerator,
            scale_denominator: display.scale_denominator,
        });

        Ok(handle)
    }

    /// Process pointer release.
    /// Validates that:
    /// 1. Target display was not removed during drag/click.
    /// 2. Scale factor did not change during click.
    /// 3. Geometry generation did not advance.
    ///
    /// Clears drag/click state on both success and error.
    pub fn on_pointer_release(
        &mut self,
        _button: PointerButton,
        _x: i32,
        _y: i32,
        geometry: DisplayGeometryGeneration,
        catalog: &Catalog,
    ) -> Result<u128, DisplayMappingError> {
        let click = self.active_click.take();
        let drag = self.active_drag.take();

        if let Some(click) = click {
            if let Some(cat_gen) = catalog.geometry_generation()
                && cat_gen != click.geometry
            {
                return Err(DisplayMappingError::StaleGeometry);
            }
            if geometry != click.geometry {
                return Err(DisplayMappingError::StaleGeometry);
            }

            let Some(display) = catalog.find(click.target_display) else {
                return Err(DisplayMappingError::DisplayNotFound);
            };

            // Fault test invariant: Change scale during a click
            if display.scale_numerator != click.scale_numerator
                || display.scale_denominator != click.scale_denominator
            {
                return Err(DisplayMappingError::StaleGeometry);
            }

            return Ok(click.target_display);
        }

        if let Some(drag) = drag {
            if let Some(cat_gen) = catalog.geometry_generation()
                && cat_gen != drag.geometry
            {
                return Err(DisplayMappingError::StaleGeometry);
            }
            if geometry != drag.geometry {
                return Err(DisplayMappingError::StaleGeometry);
            }

            let Some(display) = catalog.find(drag.target_display) else {
                return Err(DisplayMappingError::DisplayNotFound);
            };

            return Ok(display.handle);
        }

        self.explicit_target
            .ok_or(DisplayMappingError::DisplayNotFound)
    }

    /// Abort any active drag/click without execution (e.g. on focus loss or local cleanup).
    pub fn abort_interaction(&mut self) {
        self.active_drag = None;
        self.active_click = None;
    }
}
