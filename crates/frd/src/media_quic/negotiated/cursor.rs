//! Remote cursor lanes on the ORIGINAL completed media attachments. Shapes use
//! the reliable media-configuration lane (the host's `DecoderConfiguration`
//! stream, bounded by its own admitted allowance); positions use the video
//! datagram route. Nothing is available unless `remote-cursor` was selected.
use super::{Error, NegotiatedMedia};
use fr_core::limits::ProtocolLimits;
use fr_transport::quic::{DatagramRoute, QuicRecords, StreamRoute};
use fr_wire::decoder::Binding;

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
