//! The viewer's remote cursor (plan §11.4): reliable shapes and replaceable
//! positions feed ONE bounded tracker, and exactly one renderer draws it.
//!
//! Observing, the presenter composites the single client-rendered overlay
//! between decode jobs (the host capture excludes the pointer). Controlling,
//! the local pointer also renders the remote pointer, so
//! `fr_client::cursor::owner` hands rendering to EITHER the platform's local
//! pointer (with the host's confirmed shape) OR the overlay at the confirmed
//! position with the local pointer blanked, sequenced so both never draw.
//!
//! Cursor records are never media progress, freshness, decode or input
//! evidence; they bypass the receive pipeline entirely. Records are accepted
//! only on the exact negotiated routes, and only after `remote-cursor` was
//! positively selected.
use super::{Error, StreamingViewer};
use crate::media_quic::{CursorLanes, NegotiatedMedia};
use crate::session_startup::viewer::controlled::local_cursor::{Image, LocalCursor, Refused};
use asupersync::cx::Cx;
use fr_client::cursor::owner::{
    At, Confirmed, Inputs, PointerHistory, Rendered, Step, WindowCursor, resolve,
};
use fr_media::{
    cursor::{FALLBACK_SHAPE_ID, Refusal},
    pipeline::{CursorPipelineTracker, CursorRenderingOwner, PipelineError, StoredCursorShape},
    worker::overlay::Update,
};
use fr_transport::quic::{QuicRecords, Route};
use fr_wire::{Kind, WireError, cursor as wire};

/// Bounded work per call: every step is at most one presenter exchange or
/// one nonblocking platform request.
const MAX_STEPS: usize = 4;

/// Typed cursor refusal from the authenticated host's records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Wire(WireError),
    Shape(Refusal),
}

pub(super) struct ViewerCursor {
    lanes: CursorLanes,
    remote: Remote,
}
/// The bounded confirmed state and its single rendering owner.
struct Remote {
    tracker: CursorPipelineTracker,
    /// What the presenter and the platform owner have acknowledged.
    rendered: Rendered,
    /// Shape ID whose image the presenter holds.
    installed: Option<u32>,
    /// The platform owner stopped or refused: never managed again.
    local_lost: bool,
}
impl std::fmt::Debug for ViewerCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerCursor")
            .field("cached", &self.remote.tracker.usage())
            .finish_non_exhaustive()
    }
}
impl ViewerCursor {
    /// `Ok(None)`: the host did not select `remote-cursor` (typed absence).
    pub(super) fn attach(media: &NegotiatedMedia, q: &QuicRecords) -> Result<Option<Self>, Error> {
        let Some(lanes) = media.cursor_lanes(q).map_err(Error::Routes)? else {
            return Ok(None);
        };
        Ok(Some(Self {
            remote: Remote::new(lanes.view.geometry),
            lanes,
        }))
    }
    /// Follow the current media. Fresh bindings (recovery) mean an empty cache
    /// and sequence space; the presenter's old overlay is re-addressed.
    pub(super) fn refresh(
        &mut self,
        media: &NegotiatedMedia,
        q: &QuicRecords,
    ) -> Result<(), Error> {
        let lanes = media
            .cursor_lanes(q)
            .map_err(Error::Routes)?
            .ok_or(Error::Routes(crate::media_quic::Error::InvalidRoutes))?;
        if lanes.view != self.lanes.view {
            self.remote.tracker.reset(lanes.view.geometry);
            // A drawn overlay belongs to the old view: re-address it (hide).
            // Nothing is sent to a presenter that never composited a cursor.
            self.remote.rendered.overlay_unknown();
            self.remote.installed = None;
        }
        self.lanes = lanes;
        Ok(())
    }
    /// Exact negotiated route AND cursor kind, checked before any parsing.
    pub(super) fn owns(&self, route: Route, bytes: &[u8]) -> bool {
        let kind = bytes.get(6..8).map(|k| u16::from_be_bytes([k[0], k[1]]));
        (route == Route::Stream(self.lanes.shape) && kind == Some(Kind::CursorShape as u16))
            || (route == Route::Datagram(self.lanes.position)
                && kind == Some(Kind::CursorPosition as u16))
    }
    /// Validate before allocation. A stale or geometry-fenced replaceable
    /// position is dropped (a newer one supersedes it); malformed or hostile
    /// records are a typed refusal of the session's cursor lane.
    pub(super) fn receive(&mut self, route: Route, bytes: &[u8]) -> Result<(), Fault> {
        if route == Route::Stream(self.lanes.shape) {
            let shape = wire::decode_shape_record(
                bytes,
                self.lanes.shape.binding,
                self.lanes.shape.maximum,
                &self.lanes.limits,
            )
            .map_err(Fault::Wire)?;
            self.remote
                .tracker
                .store_shape(&shape)
                .map_err(|e| fault(&e))?;
            return Ok(());
        }
        let position = wire::decode_position_record(
            bytes,
            self.lanes.position.binding,
            self.lanes.position_maximum,
        )
        .map_err(Fault::Wire)?;
        match self.remote.tracker.process_position(&position) {
            Ok(_)
            | Err(PipelineError::StaleSequence { .. } | PipelineError::MismatchedGeometry { .. }) => {
                Ok(())
            }
            Err(error) => Err(fault(&error)),
        }
    }
}
impl Remote {
    fn new(geometry: fr_core::ids::DisplayGeometryGeneration) -> Self {
        Self {
            tracker: CursorPipelineTracker::new(CursorRenderingOwner::ClientRendered, geometry),
            // A freshly started presenter composites nothing.
            rendered: Rendered::new(),
            installed: None,
            local_lost: false,
        }
    }
    /// The latest confirmed host state resolved against the bounded cache: an
    /// unknown shape is the built-in fallback; hidden, locked and
    /// host-composited cursors are never drawn by this client.
    fn confirmed(&self) -> Confirmed {
        match self.tracker.current() {
            None => Confirmed::Unknown,
            Some(e) if !e.visible => Confirmed::Hidden,
            Some(e) => Confirmed::Visible {
                shape: e.shape_id,
                x: e.x,
                y: e.y,
            },
        }
    }
    fn shape(&self, id: u32) -> Option<&StoredCursorShape> {
        self.tracker
            .shape(id)
            .or_else(|| self.tracker.shape(FALLBACK_SHAPE_ID))
    }
    /// The presenter update for one overlay step.
    fn overlay_update(&self, overlay: Option<At>) -> Option<Update<'_>> {
        let Some(at) = overlay else {
            return Some(Update::Hidden);
        };
        if self.installed == Some(at.shape) {
            return Some(Update::Move { x: at.x, y: at.y });
        }
        let s = self.shape(at.shape)?;
        Some(Update::Shape {
            x: at.x,
            y: at.y,
            width: s.width,
            height: s.height,
            hotspot_x: s.hotspot_x,
            hotspot_y: s.hotspot_y,
            rgba: s.rgba(),
        })
    }
    fn overlay_applied(&mut self, overlay: Option<At>) {
        if let Some(at) = overlay {
            self.installed = Some(at.shape);
        }
        self.rendered.overlay_applied(overlay);
    }
    fn window_image(&self, cursor: WindowCursor) -> Option<Image<'_>> {
        Some(match cursor {
            WindowCursor::Default => Image::Default,
            WindowCursor::Blank => Image::Blank,
            WindowCursor::Shape(id) => {
                let s = self.shape(id)?;
                Image::Shape {
                    width: s.width,
                    height: s.height,
                    hotspot_x: s.hotspot_x,
                    hotspot_y: s.hotspot_y,
                    rgba: s.rgba(),
                }
            }
        })
    }
    /// Resolve the single owner and advance toward it. Local pointer requests
    /// are nonblocking and taken here; the only step returned is an overlay
    /// (applied on the presenter and acknowledged before the next call) or
    /// `Idle`.
    fn advance(
        &mut self,
        controlling: bool,
        mut local: Option<&mut (dyn LocalCursor + 'static)>,
        history: Option<&PointerHistory>,
        now_us: u64,
    ) -> Step {
        for _ in 0..MAX_STEPS {
            let mut pointer = None;
            if let Some(owner) = local.as_deref().filter(|_| !self.local_lost) {
                let state = owner.state();
                if state.stopped {
                    self.local_lost = true;
                    self.rendered.window_detached();
                } else {
                    self.rendered.window_attached();
                    self.rendered.window_applied(state.applied);
                    pointer = Some(state.pointer);
                }
            }
            let confirmed = self.confirmed();
            let explained = match confirmed {
                Confirmed::Visible { x, y, .. } => {
                    history.is_some_and(|h| h.explains(x, y, now_us))
                }
                Confirmed::Unknown | Confirmed::Hidden => false,
            };
            let target = resolve(Inputs {
                controlling,
                window: pointer,
                confirmed,
                explained,
            });
            match self.rendered.next(&target) {
                step @ (Step::Idle | Step::Overlay(_)) => return step,
                Step::Window(cursor) => {
                    let (Some(owner), Some(image)) =
                        (local.as_deref_mut(), self.window_image(cursor))
                    else {
                        return Step::Idle;
                    };
                    match owner.request(image) {
                        Ok(generation) => self.rendered.window_requested(cursor, generation),
                        Err(Refused::Busy) => return Step::Idle,
                        Err(Refused::Invalid | Refused::Stopped) => {
                            owner.stop();
                            self.local_lost = true;
                            self.rendered.window_detached();
                        }
                    }
                }
            }
        }
        Step::Idle
    }
}
fn fault(error: &PipelineError) -> Fault {
    match *error {
        PipelineError::Cursor(refusal) => Fault::Shape(refusal),
        PipelineError::Wire(error) => Fault::Wire(error),
        _ => Fault::Wire(WireError::InvalidValue),
    }
}

impl StreamingViewer {
    /// Advance the single remote-cursor owner while NO decode job borrows the
    /// presenter. An overlay change re-composites the retained picture; this
    /// is not a decode, presentation or visibility receipt and never touches
    /// the receiver. Local pointer requests never wait on the platform.
    pub(super) async fn apply_cursor(&mut self, cx: &Cx) -> Result<(), Error> {
        let Some(cursor) = &mut self.cursor else {
            return Ok(());
        };
        let cursor = &mut cursor.remote;
        let now = super::now(cx).map_err(Error::Session)?;
        // Only an active grant makes the local pointer a remote-cursor owner.
        let (controlling, mut local, history) = match self.peer.controlled() {
            Some(viewer) => {
                let (local, history) = viewer.local_cursor();
                (true, local, Some(history))
            }
            None => (false, None, None),
        };
        for _ in 0..MAX_STEPS {
            let Step::Overlay(overlay) =
                cursor.advance(controlling, local.as_deref_mut(), history, now)
            else {
                return Ok(());
            };
            let Some(update) = cursor.overlay_update(overlay) else {
                return Ok(());
            };
            self.presenter
                .apply_cursor(cx, &update)
                .await
                .map_err(Error::Media)?;
            cursor.overlay_applied(overlay);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
