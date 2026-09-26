//! The ONE renderer of the remote pointer while this client controls the host
//! (plan §11.4). Pure and clock-injected: no platform, transport or authority.
//!
//! While controlling, the local pointer over the viewer window drives the
//! host's pointer through absolute input, so drawing the forwarded overlay AND
//! the local pointer would show two cursors. Exactly one owner renders:
//!
//! - [`WindowCursor::Shape`]: the local pointer is the owner. The platform
//!   draws it with the host's CONFIRMED shape at zero local latency; the
//!   presenter composites nothing.
//! - An overlay at the confirmed host position, with the local pointer blanked,
//!   when the host pointer is somewhere this client's input does not explain
//!   (the host moved, warped or confined it) or the local pointer left the
//!   viewer window. The prediction is hidden, not reconciled by guessing.
//! - Neither, when the host reports its pointer hidden, outside the shared view
//!   or already composited into the pixels.
//!
//! "Explained" compares confirmed host positions with positions this client
//! actually encoded for the host, never with the current local position: during
//! motion the confirmed position always lags by a round trip and one sample, and
//! that lag is not divergence. A confirmed position is never an input command,
//! and nothing here synthesizes or replays input.
//!
//! [`Rendered`] sequences the two owners across their separate processes so a
//! transition never shows both: an overlay is removed (and acknowledged) before
//! the local pointer takes the shape, and appears only after the platform has
//! acknowledged blanking the local pointer.

/// Recent local submissions retained for [`PointerHistory::explains`].
pub const HISTORY_POSITIONS: usize = 64;
/// How long a non-latest submission can still explain a confirmed position.
/// Input tickets expire within 1.5 s, so the host cannot act on older ones.
pub const HISTORY_US: u64 = 2_000_000;

/// Bounded ring of positions this client encoded for the host, in capture
/// pixels relative to the selected display (the space of `CursorPosition`).
/// Intentionally no coordinates in Debug: input positions are not diagnostics.
#[derive(Clone)]
pub struct PointerHistory {
    ring: [(i32, i32, u64); HISTORY_POSITIONS],
    len: usize,
    next: usize,
    latest: Option<(i32, i32)>,
}
impl Default for PointerHistory {
    fn default() -> Self {
        Self {
            ring: [(0, 0, 0); HISTORY_POSITIONS],
            len: 0,
            next: 0,
            latest: None,
        }
    }
}
impl core::fmt::Debug for PointerHistory {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PointerHistory")
            .field("retained", &self.len)
            .finish_non_exhaustive()
    }
}
impl PointerHistory {
    /// One position actually encoded for the host at `at_us`. The oldest entry
    /// is overwritten; the count never grows past [`HISTORY_POSITIONS`].
    pub fn submitted(&mut self, x: i32, y: i32, at_us: u64) {
        self.ring[self.next] = (x, y, at_us);
        self.next = (self.next + 1) % HISTORY_POSITIONS;
        self.len = (self.len + 1).min(HISTORY_POSITIONS);
        self.latest = Some((x, y));
    }
    pub const fn len(&self) -> usize {
        self.len
    }
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Whether confirmed host state at `(x, y)` is explained by this client's
    /// own input: its latest submission (whatever its age: an idle pointer
    /// stays where it was sent) or one submitted within [`HISTORY_US`].
    /// Absolute input lands on the exact encoded pixel, so this is equality.
    pub fn explains(&self, x: i32, y: i32, now_us: u64) -> bool {
        if self.latest == Some((x, y)) {
            return true;
        }
        self.ring[..self.len].iter().any(|&(px, py, at)| {
            (px, py) == (x, y) && now_us.checked_sub(at).is_some_and(|age| age <= HISTORY_US)
        })
    }
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The platform's latest observation of the local pointer over the viewer
/// window (crossing events), not a prediction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalPointer {
    /// Not observed yet; treated like inside (the local pointer may be there).
    Unknown,
    Inside,
    Outside,
}

/// Latest confirmed host pointer state, already resolved against the bounded
/// shape cache (an unknown shape arrives here as the built-in fallback ID).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confirmed {
    /// No confirmed position yet.
    Unknown,
    /// Hidden, outside the shared view, locked or host-composited: no client
    /// cursor may be drawn for it.
    Hidden,
    Visible {
        shape: u32,
        x: i32,
        y: i32,
    },
}

/// An overlay at a confirmed host position, in decoded picture pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct At {
    pub shape: u32,
    pub x: i32,
    pub y: i32,
}

/// The image of the local pointer over the viewer window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowCursor {
    /// The platform's own pointer, untouched by the remote state.
    Default,
    /// Fully transparent: the local pointer is not drawn.
    Blank,
    /// The host's confirmed shape (or the built-in fallback).
    Shape(u32),
}
impl WindowCursor {
    /// Whether this local pointer image draws something.
    pub const fn draws(self) -> bool {
        !matches!(self, Self::Blank)
    }
}

/// Inputs of one resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inputs {
    /// The local pointer drives the host (an active control grant).
    pub controlling: bool,
    /// `Some` when a platform owner of the window's pointer image exists.
    pub window: Option<LocalPointer>,
    pub confirmed: Confirmed,
    /// [`PointerHistory::explains`] for the confirmed visible position.
    pub explained: bool,
}

/// What should be rendered. `window: None` leaves the platform pointer alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub overlay: Option<At>,
    pub window: Option<WindowCursor>,
    /// Controlling with a platform owner: the local pointer and the overlay
    /// both render the remote pointer, so at most one may draw.
    pub exclusive: bool,
}

/// Resolve the single owner. While controlling with a platform owner, a
/// visible overlay always comes with a blank local pointer, and a shaped local
/// pointer always comes without an overlay.
pub fn resolve(inputs: Inputs) -> Target {
    let visible = match inputs.confirmed {
        Confirmed::Visible { shape, x, y } => Some(At { shape, x, y }),
        Confirmed::Unknown | Confirmed::Hidden => None,
    };
    if !inputs.controlling {
        // Observation: the local pointer is the viewer's own, not a renderer
        // of the remote cursor. The overlay is the only remote cursor.
        return Target {
            overlay: visible,
            window: inputs.window.map(|_| WindowCursor::Default),
            exclusive: false,
        };
    }
    let Some(local) = inputs.window else {
        // No platform owner: the platform pointer is always drawn and cannot
        // carry the shape. It stays the owner while the host follows it; an
        // unexplained host position is still shown (degraded, not hidden).
        return Target {
            overlay: visible.filter(|_| !inputs.explained),
            window: None,
            exclusive: false,
        };
    };
    let (overlay, window) = match (inputs.confirmed, visible) {
        (Confirmed::Unknown, _) => (None, WindowCursor::Default),
        (_, None) => (None, WindowCursor::Blank),
        (_, Some(at)) if local == LocalPointer::Outside || !inputs.explained => {
            (Some(at), WindowCursor::Blank)
        }
        (_, Some(at)) => (None, WindowCursor::Shape(at.shape)),
    };
    Target {
        overlay,
        window: Some(window),
        exclusive: true,
    }
}

/// The next single action toward a [`Target`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Apply this overlay on the presenter and report it with
    /// [`Rendered::overlay_applied`] once acknowledged.
    Overlay(Option<At>),
    /// Request this local pointer image; report [`Rendered::window_requested`].
    Window(WindowCursor),
    /// Nothing to do, or waiting for a platform acknowledgement.
    Idle,
}

/// The presenter's acknowledged overlay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    /// Not known (for example after a view change): re-addressed, never trusted.
    Unknown,
    Hidden,
    Shown(At),
}
impl From<Option<At>> for Overlay {
    fn from(overlay: Option<At>) -> Self {
        overlay.map_or(Self::Hidden, Self::Shown)
    }
}

/// What each owner has actually applied, as acknowledged by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rendered {
    overlay: Overlay,
    /// `None`: no platform owner is managed.
    window: Option<WindowCursor>,
    requested: Option<(WindowCursor, u64)>,
}
impl Default for Rendered {
    fn default() -> Self {
        Self::new()
    }
}
impl Rendered {
    /// A freshly started presenter composites nothing.
    pub const fn new() -> Self {
        Self {
            overlay: Overlay::Hidden,
            window: None,
            requested: None,
        }
    }
    pub const fn overlay(&self) -> Overlay {
        self.overlay
    }
    pub const fn window(&self) -> Option<WindowCursor> {
        self.window
    }
    /// The next step. Order is what keeps one owner across two processes:
    /// hiding the overlay first, then the local pointer image, and an overlay
    /// only once a blank local pointer is acknowledged (or unmanaged).
    pub fn next(&self, target: &Target) -> Step {
        let drawing = self.window.is_some_and(WindowCursor::draws)
            || self.requested.is_some_and(|(w, _)| w.draws());
        let overlay_drawn = self.overlay != Overlay::Hidden;
        if overlay_drawn && (target.overlay.is_none() || (target.exclusive && drawing)) {
            // Hiding is always safe. It also ends an observation-era pair (the
            // platform pointer beside the overlay) as control takes over.
            return Step::Overlay(None);
        }
        if let Some(wanted) = target.window {
            let latest = self.requested.map(|(w, _)| w).or(self.window);
            if latest != Some(wanted) {
                return Step::Window(wanted);
            }
            if self.window != Some(wanted) {
                // Requested, not yet applied: wait before any overlay.
                return Step::Idle;
            }
        }
        match target.overlay {
            // Never beside a local pointer that draws the remote cursor.
            Some(_) if target.exclusive && drawing => Step::Idle,
            Some(at) if self.overlay != Overlay::Shown(at) => Step::Overlay(Some(at)),
            _ => Step::Idle,
        }
    }
    pub fn overlay_applied(&mut self, overlay: Option<At>) {
        self.overlay = overlay.into();
    }
    /// The presenter's overlay no longer belongs to the current view.
    pub fn overlay_unknown(&mut self) {
        if self.overlay != Overlay::Hidden {
            self.overlay = Overlay::Unknown;
        }
    }
    /// A platform owner was attached; the window still shows its own pointer.
    pub fn window_attached(&mut self) {
        if self.window.is_none() {
            self.window = Some(WindowCursor::Default);
            self.requested = None;
        }
    }
    /// The platform owner failed or stopped: it is no longer managed.
    pub fn window_detached(&mut self) {
        self.window = None;
        self.requested = None;
    }
    pub fn window_requested(&mut self, cursor: WindowCursor, generation: u64) {
        self.requested = Some((cursor, generation));
    }
    /// The platform applied every request up to `generation`.
    pub fn window_applied(&mut self, generation: u64) {
        if let Some((cursor, requested)) = self.requested
            && generation >= requested
        {
            self.window = Some(cursor);
            self.requested = None;
        }
    }
    /// The single-owner invariant over acknowledged-or-requested state: a
    /// managed local pointer that draws (or may already draw) never coexists
    /// with an overlay that is, or may still be, drawn.
    pub fn exclusive(&self) -> bool {
        let drawing = self.window.is_some_and(WindowCursor::draws)
            || self.requested.is_some_and(|(w, _)| w.draws());
        !(drawing && self.overlay != Overlay::Hidden)
    }
}

#[cfg(test)]
mod tests;
