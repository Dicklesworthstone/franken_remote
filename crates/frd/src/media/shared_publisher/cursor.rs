//! Remote cursor forwarding for the ONE original capture source (plan §11.4).
//!
//! The source samples the host's logical cursor at capture cadence, only while
//! at least one admitted, streaming viewer selected `remote-cursor`. Each such
//! viewer owns a [`ViewerLane`]: its current shape once on the reliable
//! media-configuration lane, then ONE replaceable position datagram. A viewer
//! in decoder startup, late join or recovery receives nothing. Sampling is
//! never capture freshness: it touches no frame ID, source progress, recovery
//! allowance or view readiness.
use super::{Entry, Error, Members, ObservationControl, Publisher, SendReport};
use asupersync::cx::Cx;
use fr_media::cursor::{HostCursor, Next, SourceState, ViewerLane};
use fr_transport::quic::{self, QuicRecords, Route};
use fr_wire::{cursor as wire, decoder::Binding};

/// Retained-record deadline for a reliable shape (the transport's own bound).
const SHAPE_SEND_US: u64 = 2_000_000;
/// Admission window for a replaceable position datagram.
const POSITION_SEND_US: u64 = 100_000;

/// Per-viewer cursor state, bound to the exact media view it was sent on.
#[derive(Debug, Default)]
pub(super) struct EntryCursor {
    lane: ViewerLane,
    view: Option<Binding>,
}

impl Entry {
    /// Admitted, steady observation on live original media only.
    fn cursor_streaming(&self) -> bool {
        self.failure.is_none()
            && self.join.is_none()
            && self.starting.is_none()
            && self.recovery.is_none()
            && self
                .media
                .as_ref()
                .is_some_and(crate::media_quic::NegotiatedMedia::cursor_selected)
    }
    /// At most one shape and one position per call. Typed refusals (an image
    /// larger than this viewer's reliable record bound, exhausted sequences)
    /// degrade to the fallback or to silence; they never end the viewer.
    pub(super) fn service_cursor(
        &mut self,
        cx: &Cx,
        transport: &mut QuicRecords,
        host: &HostCursor,
        owner: &ObservationControl,
        report: &mut SendReport,
    ) -> Result<(), Error> {
        if !self.cursor_streaming() {
            return Ok(());
        }
        let media = self.media.as_ref().ok_or(Error::Closed)?;
        let Some(lanes) = media.cursor_lanes(transport).map_err(Error::Transport)? else {
            return Ok(());
        };
        if self.cursor.view != Some(lanes.view) {
            // Fresh attachments mean an empty viewer cache and sequence space.
            self.cursor.lane.reset();
            self.cursor.view = Some(lanes.view);
            self.cursor.lane.observe(host.target());
        }
        let now = owner.check().map_err(Error::Media)?.as_micros();
        let control = self.control.clone();
        let mut authorize = || owner.check().is_ok() && control.check().is_ok();
        let lane = &mut self.cursor.lane;
        if let Next::Shape(id) = lane.next(now) {
            let Some(shape) = host.shape(id) else {
                // Superseded image: the next sample supplies a current target.
                return Ok(());
            };
            let bytes = wire::shape_record_bytes(shape.width, shape.height)
                .filter(|&n| {
                    n <= lanes.shape.maximum
                        && n <= lanes.limits.max_control_message_bytes() as usize
                })
                .and_then(|n| {
                    let mut record = Vec::new();
                    record.try_reserve_exact(n).ok()?;
                    record.resize(n, 0);
                    let len = wire::encode_cursor_shape(
                        &shape.wire(),
                        lanes.shape.binding,
                        &lanes.limits,
                        &mut record,
                    )
                    .ok()?;
                    (len == n).then_some(record)
                });
            let Some(record) = bytes else {
                // Typed degradation: positions name the viewer's fallback.
                lane.shape_undeliverable(id);
                return Ok(());
            };
            match transport.send(
                cx,
                Route::Stream(lanes.shape),
                &record,
                now.saturating_add(SHAPE_SEND_US),
                &mut authorize,
            ) {
                Ok(()) => {
                    report.accepted += 1;
                    if lane.shape_delivered(id, shape.rgba().len()).is_err() {
                        lane.shape_undeliverable(id);
                    }
                }
                Err(quic::Error::Backpressure) => {
                    report.pending = true;
                    return Ok(());
                }
                Err(error) => {
                    return Err(Error::Transport(crate::media_quic::Error::Transport(error)));
                }
            }
        }
        if lane.next(now) != Next::Position {
            return Ok(());
        }
        let Ok(Some(position)) = lane.position(lanes.view.geometry.as_raw()) else {
            // No target, or sequences exhausted: forwarding stops, typed.
            return Ok(());
        };
        let mut record = [0_u8; wire::CURSOR_POSITION_RECORD_BYTES];
        let Ok(len) = wire::encode_cursor_position(
            &position,
            lanes.position.binding,
            lanes.position_maximum,
            &mut record,
        ) else {
            return Ok(());
        };
        match transport.send(
            cx,
            Route::Datagram(lanes.position),
            &record[..len],
            now.saturating_add(POSITION_SEND_US),
            &mut authorize,
        ) {
            Ok(()) => {
                report.accepted += 1;
                lane.position_sent(&position, now);
            }
            // Replaceable: the next turn sends the then-latest state instead.
            Err(quic::Error::Backpressure) => report.pending = true,
            Err(error) => {
                return Err(Error::Transport(crate::media_quic::Error::Transport(error)));
            }
        }
        Ok(())
    }
}

impl Members {
    fn cursor_demand(&self) -> bool {
        self.cursor.state() == SourceState::Forwarding
            && self.entries.iter().flatten().any(Entry::cursor_streaming)
    }
}

impl Publisher {
    /// One bounded cursor sample on the original source, between captures and
    /// only with real demand; a source without a separate cursor (typed
    /// `Unsupported`) is never polled again. Worker failure is terminal like
    /// a failed capture, but typed cursor states never end the publication.
    pub(super) async fn sample_cursor(&mut self) -> Result<(), Error> {
        let owner = {
            let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
            members.tick()?;
            if !members.cursor_demand() {
                return Ok(());
            }
            members.owner.clone()
        };
        let reply = self
            .source
            .read_cursor(&owner)
            .await
            .map_err(Error::Media)?;
        let observation = reply.observation(&self.source).map_err(Error::Media)?;
        let mut members = self.members.lock().map_err(|_| Error::Poisoned)?;
        members.tick()?;
        // A refused image is hidden by the owner itself; not a capture failure.
        let _ = members.cursor.observe(&observation);
        let target = members.cursor.target();
        for entry in members.entries.iter_mut().flatten() {
            if entry.cursor_streaming() {
                entry.cursor.lane.observe(target);
            }
        }
        Ok(())
    }
}
