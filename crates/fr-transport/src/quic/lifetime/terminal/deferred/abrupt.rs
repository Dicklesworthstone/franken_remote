//! Native backlog cannot carry a terminal FRD0 record safely. After the original
//! owner fences authority, send at most one protected `CONNECTION_CLOSE` instead.
//! The pinned native close path enters Draining before flushing, discards queued
//! protected application packets, and emits only the close. Never call flush or
//! `drive_io_once` here: neither is a general post-fence send permission.
use super::super::{Binding, Closed, DRAIN_US, Error, QuicRecords, Report, StreamRoute, now};
use asupersync::{
    cx::Cx,
    net::quic_native::{NativeQuicUdpConnection, QuicConnectionState},
    time::timeout,
};
use std::{sync::Arc, time::Duration};

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
    native: NativeQuicUdpConnection,
    cx: Cx,
    gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    binding: Binding,
    code: u64,
    started: u64,
    until: u64,
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
        cleanup.checkpoint().map_err(|_| Error::Cancelled)?;
        let until = started.checked_add(DRAIN_US).ok_or(Error::Clock)?;
        let current = now(cleanup)?;
        if current < started || self.last_now.is_some_and(|last| started < last) {
            return Err(Error::Clock);
        }
        if current >= until {
            return Err(Error::Expired);
        }
        if self
            .terminal_lifetime_check
            .as_ref()
            .is_some_and(|gate| !gate())
        {
            return Err(Error::Unauthorized);
        }
        // The same already-registered host control route is required. There is
        // no new connection, socket, authority callback, or ordinary I/O owner.
        self.terminal_route(route, binding, fr_wire::closure::CLOSED_BYTES)?;
        Ok(ClosePacket {
            native: self.native.take().ok_or(Error::Closed)?,
            cx: cleanup.clone(),
            gate: self.terminal_lifetime_check.clone(),
            binding,
            code,
            started,
            until,
        })
    }
}
impl ClosePacket {
    pub(super) fn replace_report(&mut self, report: Closed) -> Result<(), Error> {
        // ClosedReport must replace the private construction placeholder with
        // the original owner's final reason. This does not refresh the budget.
        self.code = report_code(&Report::Closed(report), self.binding)?;
        Ok(())
    }
    pub(super) async fn run(mut self: Box<Self>) -> Result<(), Error> {
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        if self.gate.as_ref().is_some_and(|gate| !gate()) {
            return Err(Error::Unauthorized);
        }
        let at = now(&self.cx)?;
        if at < self.started {
            return Err(Error::Clock);
        }
        if at >= self.until {
            return Err(Error::Expired);
        }
        if self.native.connection().state() != QuicConnectionState::Established {
            return Err(Error::Closed);
        }
        let cx = self.cx.clone();
        let remaining = Duration::from_micros(self.until - at);
        let gate = self.gate.clone();
        let until = self.until;
        let code = self.code;
        let sent = timeout(
            cx.now(),
            remaining,
            super::super::super::poll_io(
                &cx,
                gate.as_deref(),
                &mut || true,
                // Native close discards protected application packets first;
                // this detached owner can send only the terminal close packet.
                &[],
                at,
                Some(until),
                self.native.close(&cx, code),
            ),
        )
        .await
        .map_err(|_| Error::Expired)??;
        if sent == 0 {
            return Err(Error::Closed);
        }
        // A flushed close is neither an ACK nor delivery of the FRD0 report.
        // Preserve its original refusal; no cleanup/effect stages are inferred.
        Err(Error::Backpressure)
    }
}
