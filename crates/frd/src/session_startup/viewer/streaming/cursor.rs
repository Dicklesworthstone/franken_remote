//! The viewer's remote cursor (plan §11.4): reliable shapes and replaceable
//! positions feed ONE bounded tracker, and the single client-rendered overlay
//! is installed on the presenter between decode jobs. The host capture
//! excludes the pointer, so this is the only cursor drawn for the desktop.
//!
//! Cursor records are never media progress, freshness, decode or input
//! evidence; they bypass the receive pipeline entirely. Records are accepted
//! only on the exact negotiated routes, and only after `remote-cursor` was
//! positively selected.
use super::{Error, StreamingViewer};
use crate::media_quic::{CursorLanes, NegotiatedMedia};
use asupersync::cx::Cx;
use fr_media::{
    cursor::Refusal,
    pipeline::{CursorPipelineTracker, CursorRenderingOwner, PipelineError},
    worker::overlay::Update,
};
use fr_transport::quic::{QuicRecords, Route};
use fr_wire::{Kind, WireError, cursor as wire};

/// Typed cursor refusal from the authenticated host's records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Wire(WireError),
    Shape(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shown {
    Hidden,
    At { shape: u32, x: i32, y: i32 },
}

pub(super) struct ViewerCursor {
    tracker: CursorPipelineTracker,
    lanes: CursorLanes,
    /// What the presenter currently composites; `None` is unknown (re-send).
    shown: Option<Shown>,
    /// Shape ID whose image the presenter holds.
    installed: Option<u32>,
}
impl std::fmt::Debug for ViewerCursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerCursor")
            .field("cached", &self.tracker.usage())
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
            tracker: CursorPipelineTracker::new(
                CursorRenderingOwner::ClientRendered,
                lanes.view.geometry,
            ),
            lanes,
            // A freshly started presenter composites nothing.
            shown: Some(Shown::Hidden),
            installed: None,
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
            self.tracker.reset(lanes.view.geometry);
            // A drawn overlay belongs to the old view: re-address it (hide).
            // Nothing is sent to a presenter that never composited a cursor.
            if self.shown != Some(Shown::Hidden) {
                self.shown = None;
            }
            self.installed = None;
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
            self.tracker.store_shape(&shape).map_err(|e| fault(&e))?;
            return Ok(());
        }
        let position = wire::decode_position_record(
            bytes,
            self.lanes.position.binding,
            self.lanes.position_maximum,
        )
        .map_err(Fault::Wire)?;
        match self.tracker.process_position(&position) {
            Ok(_)
            | Err(PipelineError::StaleSequence { .. } | PipelineError::MismatchedGeometry { .. }) => {
                Ok(())
            }
            Err(error) => Err(fault(&error)),
        }
    }
    fn desired(&self) -> Shown {
        match self.tracker.current() {
            Some(e) if e.visible => Shown::At {
                shape: e.shape_id,
                x: e.x,
                y: e.y,
            },
            _ => Shown::Hidden,
        }
    }
    /// The presenter update for a changed resolved overlay, if any.
    pub(super) fn pending(&self) -> Option<Update<'_>> {
        let desired = self.desired();
        if self.shown == Some(desired) {
            return None;
        }
        match desired {
            Shown::Hidden => Some(Update::Hidden),
            Shown::At { shape, x, y } if self.installed == Some(shape) => {
                Some(Update::Move { x, y })
            }
            Shown::At { shape, x, y } => {
                let s = self.tracker.shape(shape)?;
                Some(Update::Shape {
                    x,
                    y,
                    width: s.width,
                    height: s.height,
                    hotspot_x: s.hotspot_x,
                    hotspot_y: s.hotspot_y,
                    rgba: s.rgba(),
                })
            }
        }
    }
    fn applied(&mut self) {
        let desired = self.desired();
        if let Shown::At { shape, .. } = desired {
            self.installed = Some(shape);
        }
        self.shown = Some(desired);
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
    /// Install a changed overlay while NO decode job borrows the presenter.
    /// The presenter re-composites its retained picture; this is not a decode,
    /// presentation or visibility receipt and never touches the receiver.
    pub(super) async fn apply_cursor(&mut self, cx: &Cx) -> Result<(), Error> {
        let Some(cursor) = &mut self.cursor else {
            return Ok(());
        };
        let Some(update) = cursor.pending() else {
            return Ok(());
        };
        self.presenter
            .apply_cursor(cx, &update)
            .await
            .map_err(Error::Media)?;
        cursor.applied();
        Ok(())
    }
}
