//! Remote cursor lanes on the ORIGINAL completed media attachments. Shapes use
//! the reliable media-configuration lane (the host's `DecoderConfiguration`
//! stream, bounded by its own admitted allowance); positions use the video
//! datagram route. Nothing is available unless `remote-cursor` was selected.
use super::{Error, NegotiatedMedia};
use asupersync::cx::Cx;
use fr_core::limits::ProtocolLimits;
use fr_media::cursor::{HostCursor, Next, ViewerLane};
use fr_transport::quic::{self, DatagramRoute, QuicRecords, Route, StreamRoute};
use fr_wire::{cursor as wire, decoder::Binding};

/// Retained-record deadline for a reliable shape (the transport's own bound).
const SHAPE_SEND_US: u64 = 2_000_000;
/// Admission window for a replaceable position datagram.
const POSITION_SEND_US: u64 = 100_000;

/// Exact routes and bounds for one direction of cursor forwarding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorLanes {
    /// Host outbound / viewer inbound reliable shape lane.
    pub shape: StreamRoute,
    /// Host outbound / viewer inbound replaceable position datagram route.
    pub position: DatagramRoute,
    /// Complete position-record bound on the datagram route.
    pub position_maximum: usize,
    pub limits: ProtocolLimits,
    /// The admitted view; positions carry its display-geometry generation.
    pub view: Binding,
}
impl NegotiatedMedia {
    /// Positive selection of `remote-cursor` in this media's negotiation.
    pub(crate) fn cursor_selected(&self) -> bool {
        self.selection
            .capabilities
            .iter()
            .any(|c| c.name == fr_wire::cursor::CAPABILITY && c.version == fr_wire::cursor::VERSION)
    }
    /// `Ok(None)` is typed absence: the peer did not select `remote-cursor`,
    /// so no cursor record may be sent to it or accepted from it.
    pub(crate) fn cursor_lanes(&self, q: &QuicRecords) -> Result<Option<CursorLanes>, Error> {
        self.check(q)?;
        if !self.cursor_selected() {
            return Ok(None);
        }
        let host = self.is_host();
        let position = self.video.datagram.ok_or(Error::InvalidRoutes)?;
        Ok(Some(CursorLanes {
            shape: if host {
                self.configuration.outbound
            } else {
                self.configuration.inbound
            },
            position,
            position_maximum: self.limits.record_bytes(),
            limits: self.selection.limits,
            view: self.binding(),
        }))
    }
}

/// What one bounded turn admitted to the transport. Not delivery, decode or
/// presentation evidence.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CursorTurn {
    pub accepted: usize,
    /// Backpressure: the next turn sends the then-current state instead.
    pub pending: bool,
}

/// One viewer's host-side cursor lane bound to the exact media view it was
/// sent on. A fresh attachment means an empty viewer cache and sequence space.
#[derive(Debug, Default)]
pub(crate) struct HostLane {
    pub lane: ViewerLane,
    view: Option<Binding>,
}
impl HostLane {
    /// At most one reliable shape and one replaceable position datagram for
    /// ONE admitted, streaming viewer. Typed refusals (an image larger than
    /// this viewer's reliable record bound, exhausted sequences) degrade to
    /// the viewer's fallback or to silence; they never end the viewer.
    /// `authorize` is rechecked by the transport immediately before sending.
    pub(crate) fn service(
        &mut self,
        cx: &Cx,
        transport: &mut QuicRecords,
        lanes: &CursorLanes,
        host: &HostCursor,
        now: u64,
        authorize: &mut impl FnMut() -> bool,
    ) -> Result<CursorTurn, quic::Error> {
        let mut turn = CursorTurn::default();
        if self.view != Some(lanes.view) {
            self.lane.reset();
            self.view = Some(lanes.view);
            self.lane.observe(host.target());
        }
        let lane = &mut self.lane;
        if let Next::Shape(id) = lane.next(now) {
            let Some(shape) = host.shape(id) else {
                // Superseded image: the next sample supplies a current target.
                return Ok(turn);
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
                return Ok(turn);
            };
            match transport.send(
                cx,
                Route::Stream(lanes.shape),
                &record,
                now.saturating_add(SHAPE_SEND_US),
                &mut *authorize,
            ) {
                Ok(()) => {
                    turn.accepted += 1;
                    if lane.shape_delivered(id, shape.rgba().len()).is_err() {
                        lane.shape_undeliverable(id);
                    }
                }
                Err(quic::Error::Backpressure) => {
                    turn.pending = true;
                    return Ok(turn);
                }
                Err(error) => return Err(error),
            }
        }
        if lane.next(now) != Next::Position {
            return Ok(turn);
        }
        let Ok(Some(position)) = lane.position(lanes.view.geometry.as_raw()) else {
            // No target, or sequences exhausted: forwarding stops, typed.
            return Ok(turn);
        };
        let mut record = [0_u8; wire::CURSOR_POSITION_RECORD_BYTES];
        let Ok(len) = wire::encode_cursor_position(
            &position,
            lanes.position.binding,
            lanes.position_maximum,
            &mut record,
        ) else {
            return Ok(turn);
        };
        match transport.send(
            cx,
            Route::Datagram(lanes.position),
            &record[..len],
            now.saturating_add(POSITION_SEND_US),
            &mut *authorize,
        ) {
            Ok(()) => {
                turn.accepted += 1;
                lane.position_sent(&position, now);
            }
            // Replaceable: the next turn sends the then-latest state instead.
            Err(quic::Error::Backpressure) => turn.pending = true,
            Err(error) => return Err(error),
        }
        Ok(turn)
    }
}
