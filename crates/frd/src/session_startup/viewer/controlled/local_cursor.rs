//! The platform owner of the LOCAL pointer's image over the viewer window
//! (plan §11.4). While this viewer controls the host, its local pointer drives
//! the host pointer through absolute input; `fr_client::cursor::owner` decides
//! whether that local pointer (drawn by the platform with the host's confirmed
//! shape) or the presenter's overlay renders the remote pointer, never both.
//!
//! The platform half runs on its own native thread and connection, separate
//! from input capture, codecs and QUIC. This boundary is nonblocking in both
//! directions: a request replaces at most one pending image, and the state is
//! the owner's latest acknowledged observation. It grants and carries no input.
use super::{ControlledViewer, Error};
pub use fr_client::cursor::owner::LocalPointer;
use fr_client::cursor::owner::PointerHistory;
use fr_client::input::{Action, ClientInstant};
use fr_core::input::DesktopPoint;

/// One requested local pointer image. `Shape` is straight-alpha RGBA8 whose
/// geometry the implementation validates before any allocation.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Image<'a> {
    /// The platform's own pointer.
    Default,
    /// Fully transparent: no local pointer is drawn.
    Blank,
    Shape {
        width: u16,
        height: u16,
        hotspot_x: u16,
        hotspot_y: u16,
        rgba: &'a [u8],
    },
}
impl std::fmt::Debug for Image<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Geometry only: cursor pixels never reach diagnostics.
        match self {
            Self::Default => f.write_str("Default"),
            Self::Blank => f.write_str("Blank"),
            Self::Shape { width, height, .. } => f
                .debug_struct("Shape")
                .field("width", width)
                .field("height", height)
                .finish_non_exhaustive(),
        }
    }
}

/// The owner's latest acknowledged state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    /// Latest native crossing observation of the local pointer.
    pub pointer: LocalPointer,
    /// Every request up to this generation is applied (0: none yet).
    pub applied: u64,
    /// Failed or stopped: nothing further is applied, and the owner has
    /// attempted to restore the platform's own pointer.
    pub stopped: bool,
}

/// A request that was not admitted. `Busy` is retried with the then-current
/// image on a later turn; the others end management of the local pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Busy,
    Invalid,
    Stopped,
}

/// Implementations must not wait on native calls, locks held by native code,
/// the network or a thread join, and must restore the platform's own pointer
/// when they stop. Their native thread is owned and reaped by the platform's
/// input capture attachment, not by this handle.
pub trait LocalCursor: Send + Sync {
    fn state(&self) -> State;
    /// Replace the pending image. Returns its strictly increasing generation.
    fn request(&mut self, image: Image<'_>) -> Result<u64, Refused>;
    fn stop(&self);
}

impl ControlledViewer {
    /// Attach the ONE platform owner of this window's pointer image to the
    /// ORIGINAL granted viewer. No input, grant or visibility follows from it;
    /// it stops with the viewer.
    pub fn attach_local_cursor(&mut self, owner: Box<dyn LocalCursor>) -> Result<(), Error> {
        if let Err(error) = self.check() {
            owner.stop();
            return Err(error);
        }
        if self.local_cursor.is_some() {
            owner.stop();
            return Err(Error::Capture(super::events::Error::AlreadyAttached));
        }
        self.local_cursor = Some(owner);
        Ok(())
    }
    /// The attached owner (if any) and this viewer's own recent submissions.
    pub(in crate::session_startup::viewer) fn local_cursor(
        &mut self,
    ) -> (Option<&mut (dyn LocalCursor + 'static)>, &PointerHistory) {
        (self.local_cursor.as_deref_mut(), &self.pointer_history)
    }
    /// Record a position actually encoded for the host, relative to the
    /// granted display (the space of confirmed `CursorPosition` records).
    pub(super) fn note_pointer(&mut self, position: DesktopPoint, at: ClientInstant) {
        let origin = self.viewport.bounds().origin();
        if let (Some(x), Some(y)) = (
            position.x.checked_sub(origin.x),
            position.y.checked_sub(origin.y),
        ) {
            self.pointer_history.submitted(x, y, at.0);
        }
    }
    pub(super) fn note_action(&mut self, action: &Action<'_>, at: ClientInstant) {
        if let Action::Button { position, .. } | Action::Scroll { position, .. } = *action {
            self.note_pointer(position, at);
        }
    }
    pub(super) fn stop_local_cursor(&self) {
        if let Some(owner) = &self.local_cursor {
            owner.stop();
        }
    }
}
