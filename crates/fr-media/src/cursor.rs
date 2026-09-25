//! Remote cursor forwarding state (plan §11.4; PROTOCOL.md `0x0038`/`0x0039`).
//!
//! Pure and clock-injected: no runtime, worker, transport or authority lives
//! here. The host maps native cursor serials to connection-scoped wire shape
//! IDs ([`HostCursor`]) and keeps ONE replaceable position per viewer
//! ([`ViewerLane`]). The viewer keeps the same bounded shape cache
//! ([`crate::pipeline::CursorPipelineTracker`]); the host mirrors each viewer's
//! cache with identical count/byte limits and FIFO eviction ([`ShapeSet`]), so a
//! shape the viewer must have evicted is sent again instead of being assumed.
//!
//! A cursor observation is confirmed OS pointer state for rendering only. It
//! is never source-freshness evidence, never a decode or presentation witness,
//! and never an input command.
use crate::worker::cursor::{Observation, Snapshot};
use fr_wire::cursor::{
    CursorPosition, CursorShape, MAX_CURSOR_DIMENSION, POSITION_FLAG_VISIBLE, SHAPE_FLAG_VISIBLE,
};
use std::collections::VecDeque;

/// Shapes one cache may hold (host map, per-viewer mirror and viewer cache).
pub const MAX_SHAPES: usize = 16;
/// RGBA8 bytes one cache may hold, independent of the count bound.
pub const MAX_SHAPE_BYTES: usize = 1 << 20;
/// Reserved wire ID: "use the viewer's built-in fallback". Never assigned.
pub const FALLBACK_SHAPE_ID: u32 = 0;
/// An unchanged confirmed position is re-sent at most this often, bounding the
/// damage of a lost replaceable datagram without creating idle video traffic.
pub const REFRESH_US: u64 = 1_000_000;

/// Typed refusal for hostile, unrepresentable or exhausted cursor state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    ZeroSize,
    Oversized,
    LengthMismatch,
    HotspotOutside,
    InvalidScale,
    InvalidFlags,
    ReservedShapeId,
    /// A single image larger than an entire cache's byte budget.
    ShapeExceedsCache,
    /// Wire shape IDs are never reused; exhaustion stops forwarding.
    IdsExhausted,
    /// Position sequences never wrap; exhaustion stops forwarding.
    SequenceExhausted,
}
impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for Refusal {}

/// Structural admission BEFORE any allocation: non-zero bounded dimensions,
/// an in-image hotspot and the exact checked RGBA8 length. Returns that length.
pub fn check_geometry(
    width: u16,
    height: u16,
    hotspot_x: u16,
    hotspot_y: u16,
    len: usize,
) -> Result<usize, Refusal> {
    if width == 0 || height == 0 {
        return Err(Refusal::ZeroSize);
    }
    if width > MAX_CURSOR_DIMENSION || height > MAX_CURSOR_DIMENSION {
        return Err(Refusal::Oversized);
    }
    let bytes = usize::from(width)
        .checked_mul(usize::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(Refusal::Oversized)?;
    if len != bytes {
        return Err(Refusal::LengthMismatch);
    }
    if hotspot_x >= width || hotspot_y >= height {
        return Err(Refusal::HotspotOutside);
    }
    if bytes > MAX_SHAPE_BYTES {
        return Err(Refusal::ShapeExceedsCache);
    }
    Ok(bytes)
}

/// Bounded FIFO membership of shape IDs, counted in shapes AND bytes. Used by
/// the viewer cache and, identically, by the host's per-viewer mirror.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShapeSet {
    order: VecDeque<(u32, usize)>,
    bytes: usize,
}
impl ShapeSet {
    pub fn contains(&self, id: u32) -> bool {
        self.order.iter().any(|(i, _)| *i == id)
    }
    pub fn len(&self) -> usize {
        self.order.len()
    }
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
    /// Admit a NEW id, evicting the oldest entries until both bounds hold.
    /// Returns how many were evicted (the caller evicts the same front items).
    /// A contained id is not re-admitted and does not change the order.
    pub fn admit(&mut self, id: u32, bytes: usize) -> Result<usize, Refusal> {
        if id == FALLBACK_SHAPE_ID {
            return Err(Refusal::ReservedShapeId);
        }
        if bytes > MAX_SHAPE_BYTES {
            return Err(Refusal::ShapeExceedsCache);
        }
        if self.contains(id) {
            return Ok(0);
        }
        let mut evicted = 0;
        while self.order.len() >= MAX_SHAPES || self.bytes + bytes > MAX_SHAPE_BYTES {
            let (_, old) = self.order.pop_front().expect("bounded non-empty cache");
            self.bytes -= old;
            evicted += 1;
        }
        self.order.push_back((id, bytes));
        self.bytes += bytes;
        Ok(evicted)
    }
    /// Remove one id, returning its position in FIFO order if present.
    pub fn remove(&mut self, id: u32) -> Option<usize> {
        let index = self.order.iter().position(|(i, _)| *i == id)?;
        let (_, bytes) = self.order.remove(index).expect("located");
        self.bytes -= bytes;
        Some(index)
    }
    pub fn clear(&mut self) {
        self.order.clear();
        self.bytes = 0;
    }
}

/// One host-side logical cursor image with its connection-scoped wire ID.
#[derive(Clone, PartialEq, Eq)]
pub struct HostShape {
    pub id: u32,
    native_serial: u32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    rgba: Vec<u8>,
}
impl core::fmt::Debug for HostShape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Dimensions and byte counts only: cursor pixels never reach diagnostics.
        f.debug_struct("HostShape")
            .field("id", &self.id)
            .field("width", &self.width)
            .field("height", &self.height)
            .field("bytes", &self.rgba.len())
            .finish_non_exhaustive()
    }
}
impl HostShape {
    pub fn rgba(&self) -> &[u8] {
        &self.rgba
    }
    /// The wire record. The host capture excludes the pointer, so the viewer is
    /// the single rendering owner: never `SHAPE_FLAG_HOST_COMPOSITED`.
    pub fn wire(&self) -> CursorShape<'_> {
        CursorShape {
            shape_id: self.id,
            width: self.width,
            height: self.height,
            hotspot_x: self.hotspot_x,
            hotspot_y: self.hotspot_y,
            scale_1000: 1000,
            flags: SHAPE_FLAG_VISIBLE,
            rgba: &self.rgba,
        }
    }
    fn matches(&self, s: &Snapshot<'_>) -> bool {
        self.native_serial == s.native_serial
            && (self.width, self.height, self.hotspot_x, self.hotspot_y)
                == (s.width, s.height, s.hotspot_x, s.hotspot_y)
            && self.rgba == s.rgba
    }
}

/// Latest confirmed host pointer state for the shared view, in capture pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    /// Wire shape ID, or [`FALLBACK_SHAPE_ID`].
    pub shape: u32,
    /// Hotspot position relative to the captured display.
    pub x: i32,
    pub y: i32,
    /// Pointer inside the shared view with a representable logical image.
    /// XFIXES cannot observe `XFixesHideCursor`; this is NOT certified
    /// physical visibility.
    pub visible: bool,
}

/// Forwarding state of the capture source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    Forwarding,
    /// The source cannot observe a separate cursor (e.g. no XFIXES). Typed
    /// absence: no records are produced and the source is not polled again.
    Unsupported,
    /// Wire IDs exhausted; forwarding stopped instead of reusing identities.
    Exhausted,
}

/// Publisher-level cursor owner: bounded serial→ID map plus the latest target.
#[derive(Debug)]
pub struct HostCursor {
    shapes: VecDeque<HostShape>,
    bytes: usize,
    next_id: u32,
    target: Option<Target>,
    state: SourceState,
}
impl Default for HostCursor {
    fn default() -> Self {
        Self::new()
    }
}
impl HostCursor {
    pub const fn new() -> Self {
        Self {
            shapes: VecDeque::new(),
            bytes: 0,
            next_id: 1,
            target: None,
            state: SourceState::Forwarding,
        }
    }
    pub const fn state(&self) -> SourceState {
        self.state
    }
    pub const fn target(&self) -> Option<Target> {
        self.target
    }
    pub fn shape(&self, id: u32) -> Option<&HostShape> {
        self.shapes.iter().find(|s| s.id == id)
    }
    /// Cached host images: (count, RGBA bytes). Both are bounded.
    pub fn usage(&self) -> (usize, usize) {
        (self.shapes.len(), self.bytes)
    }
    /// Fold one worker observation into the confirmed target. `Moving` keeps
    /// the previous target (no guessed coordinates); `Unsupported` is terminal
    /// typed absence. Geometry is checked before the bounded image copy.
    pub fn observe(&mut self, observation: &Observation<'_>) -> Result<Option<Target>, Refusal> {
        if self.state != SourceState::Forwarding {
            return Ok(None);
        }
        match observation {
            Observation::Moving => {}
            Observation::Unsupported => {
                self.state = SourceState::Unsupported;
                self.target = None;
            }
            Observation::Outside | Observation::Unrepresentable => {
                // Keep the last known location only to address the hide; no
                // new coordinates are invented for an unobserved pointer.
                self.target = self.target.map(|t| Target {
                    visible: false,
                    ..t
                });
            }
            Observation::Inside(snapshot) => {
                let id = match self.intern(snapshot) {
                    Ok(id) => id,
                    Err(Refusal::IdsExhausted) => {
                        self.state = SourceState::Exhausted;
                        // Hide the last drawn cursor; nothing further is sent.
                        self.target = self.target.map(|t| Target {
                            visible: false,
                            ..t
                        });
                        return Err(Refusal::IdsExhausted);
                    }
                    Err(refusal) => {
                        // A refused image is hidden, never drawn from stale state.
                        self.target = self.target.map(|t| Target {
                            visible: false,
                            ..t
                        });
                        return Err(refusal);
                    }
                };
                self.target = Some(Target {
                    shape: id,
                    x: snapshot.x,
                    y: snapshot.y,
                    visible: true,
                });
            }
        }
        Ok(self.target)
    }
    fn intern(&mut self, s: &Snapshot<'_>) -> Result<u32, Refusal> {
        let bytes = check_geometry(s.width, s.height, s.hotspot_x, s.hotspot_y, s.rgba.len())?;
        if let Some(index) = self.shapes.iter().position(|h| h.matches(s)) {
            // Least-recently-used order: a hit moves to the back.
            let hit = self.shapes.remove(index).expect("located");
            let id = hit.id;
            self.shapes.push_back(hit);
            return Ok(id);
        }
        // A reused native serial with different pixels is a NEW identity.
        if let Some(index) = self
            .shapes
            .iter()
            .position(|h| h.native_serial == s.native_serial)
        {
            let old = self.shapes.remove(index).expect("located");
            self.bytes -= old.rgba.len();
        }
        let id = self.next_id;
        let next = id.checked_add(1).ok_or(Refusal::IdsExhausted)?;
        let mut rgba = Vec::new();
        rgba.try_reserve_exact(bytes)
            .map_err(|_| Refusal::ShapeExceedsCache)?;
        rgba.extend_from_slice(s.rgba);
        while self.shapes.len() >= MAX_SHAPES || self.bytes + bytes > MAX_SHAPE_BYTES {
            let old = self.shapes.pop_front().expect("bounded non-empty cache");
            self.bytes -= old.rgba.len();
        }
        self.next_id = next;
        self.bytes += bytes;
        self.shapes.push_back(HostShape {
            id,
            native_serial: s.native_serial,
            width: s.width,
            height: s.height,
            hotspot_x: s.hotspot_x,
            hotspot_y: s.hotspot_y,
            rgba,
        });
        Ok(id)
    }
    #[cfg(test)]
    fn exhaust_for_test(&mut self) {
        self.next_id = u32::MAX;
    }
}

/// What one viewer's lane should transmit next, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Idle,
    /// Send this shape on the reliable media-configuration lane first.
    Shape(u32),
    /// Send the latest position datagram (see [`ViewerLane::position`]).
    Position,
}

/// One viewer's replaceable cursor state. Holds at most ONE pending position
/// (the latest target) and never a queue; an obsolete target is overwritten.
/// A position never references a shape before that shape was admitted to the
/// viewer's reliable lane, except the reserved fallback.
#[derive(Debug, Default, Clone)]
pub struct ViewerLane {
    delivered: ShapeSet,
    undeliverable: Option<u32>,
    target: Option<Target>,
    sent: Option<(Target, u64)>,
    sequence: u64,
    exhausted: bool,
}
impl ViewerLane {
    /// Replace the desired state with the latest confirmed target.
    pub fn observe(&mut self, target: Option<Target>) {
        self.target = target;
    }
    /// Fresh media bindings mean a fresh viewer cache and sequence space.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn delivered(&self) -> &ShapeSet {
        &self.delivered
    }
    fn wire(&self, t: Target) -> Target {
        if t.visible && self.undeliverable == Some(t.shape) {
            // Explicitly the viewer's fallback: this image cannot fit the
            // viewer's admitted reliable record bound.
            Target {
                shape: FALLBACK_SHAPE_ID,
                ..t
            }
        } else {
            t
        }
    }
    pub fn next(&self, now_us: u64) -> Next {
        if self.exhausted {
            return Next::Idle;
        }
        let Some(target) = self.target.map(|t| self.wire(t)) else {
            return Next::Idle;
        };
        if target.visible
            && target.shape != FALLBACK_SHAPE_ID
            && !self.delivered.contains(target.shape)
        {
            return Next::Shape(target.shape);
        }
        match self.sent {
            Some((sent, at)) if sent == target && now_us < at.saturating_add(REFRESH_US) => {
                Next::Idle
            }
            _ => Next::Position,
        }
    }
    /// The transport admitted this shape to the reliable lane.
    pub fn shape_delivered(&mut self, id: u32, bytes: usize) -> Result<(), Refusal> {
        self.delivered.admit(id, bytes).map(|_| ())
    }
    /// This viewer's reliable record bound cannot carry the image.
    pub fn shape_undeliverable(&mut self, id: u32) {
        self.undeliverable = Some(id);
    }
    /// Allocate the next strictly increasing sequence for the current target.
    /// A backpressured datagram simply leaves a gap; nothing is retried stale.
    pub fn position(
        &mut self,
        geometry_generation: u64,
    ) -> Result<Option<CursorPosition>, Refusal> {
        let Some(target) = self.target.map(|t| self.wire(t)) else {
            return Ok(None);
        };
        let Some(sequence) = self.sequence.checked_add(1) else {
            self.exhausted = true;
            return Err(Refusal::SequenceExhausted);
        };
        self.sequence = sequence;
        Ok(Some(CursorPosition {
            shape_id: target.shape,
            x: target.x,
            y: target.y,
            geometry_generation,
            sequence,
            flags: if target.visible {
                POSITION_FLAG_VISIBLE
            } else {
                0
            },
        }))
    }
    /// The transport admitted the datagram carrying `position`.
    pub fn position_sent(&mut self, position: &CursorPosition, now_us: u64) {
        self.sent = Some((
            Target {
                shape: position.shape_id,
                x: position.x,
                y: position.y,
                visible: position.flags & POSITION_FLAG_VISIBLE != 0,
            },
            now_us,
        ));
    }
    #[cfg(test)]
    fn exhaust_sequence_for_test(&mut self) {
        self.sequence = u64::MAX;
    }
}

#[cfg(test)]
mod tests;
