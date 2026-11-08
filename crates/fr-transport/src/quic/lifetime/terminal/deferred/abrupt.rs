//! Native backlog cannot carry a terminal FRD0 record safely. After the original
//! owner fences authority, send at most one protected `CONNECTION_CLOSE` instead.
//! The pinned native close path enters Draining before flushing, discards queued
//! protected application packets, and emits only the close. Never call flush or
//! `drive_io_once` here: neither is a general post-fence send permission.
use super::super::packet::Packet;
use super::super::{Binding, Closed, Error, QuicRecords, Report, StreamRoute};
use asupersync::cx::Cx;

// PROTOCOL_TRANSPORT_CLOSE.md: namespaces for the existing reason enums.
// Only a reason is carried. No lease, cleanup stage, receipt count or content.
pub(super) fn report_code(report: &Report, binding: Binding) -> Result<u64, Error> {
    let (code, copy) = match report {
        Report::Closed(closed) => (
            crate::quic::PeerClose::Session(closed.reason).code(),
            Report::Closed(*closed),
        ),
        Report::Revoked(revoked) => (
            crate::quic::PeerClose::Lease(revoked.reason).code(),
            Report::Revoked(*revoked),
        ),
    };
    // Keep the ordinary report's binding/canonical-metadata validation, even
    // though none of its content or cleanup claims will be sent in this mode.
    copy.encode(binding, &mut [0; super::super::REPORT_BYTES])?;
    Ok(code)
}

pub(super) struct ClosePacket {
    packet: Box<Packet>,
    binding: Binding,
}
impl QuicRecords {
    pub(super) fn prepare_close_packet(
        &mut self,
        cleanup: &Cx,
        route: StreamRoute,
        binding: Binding,
        started: u64,
        code: u64,
    ) -> Result<ClosePacket, Error> {
        // Only the same already-registered host control route can supply a
        // report. The shared packet owner does not invent a session binding.
        self.terminal_route(route, binding, fr_wire::closure::CLOSED_BYTES)?;
        Ok(ClosePacket {
            packet: Box::new(self.prepare_terminal_packet(cleanup, started, code)?),
            binding,
        })
    }
}
impl ClosePacket {
    pub(super) fn replace_report(&mut self, report: Closed) -> Result<(), Error> {
        self.packet
            .replace_code(report_code(&Report::Closed(report), self.binding)?);
        Ok(())
    }
    pub(super) async fn run(self: Box<Self>) -> Result<(), Error> {
        self.packet.run().await?;
        // A flushed close is neither an ACK nor delivery of the FRD0 report.
        // Preserve its original refusal; no cleanup/effect stages are inferred.
        Err(Error::Backpressure)
    }
}
