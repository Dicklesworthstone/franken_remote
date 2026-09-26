//! A terminal report owns the original socket, never an ordinary send permit.
//! Native retained data cannot be selectively discarded through the pinned API:
//! refuse that case rather than flushing stale application bytes after a fence.
use super::super::{
    ConnectionBinding, EmptySendState, Error, Messages, Priority, QuicRecords, Route, StreamRoute,
    now,
};
use asupersync::{
    bytes::Bytes,
    cx::Cx,
    net::quic_native::{NativeQuicUdpConnection, QuicConnectionState, StreamRole},
    time::timeout,
};
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    authority::Binding,
    closure::{self, CLOSED_BYTES, Closed},
    input::{InputDelivery, InputDirection},
    lease_revoked::{self, REVOKED_BYTES, Revoked},
};
use std::{future::Future, sync::Arc, time::Duration};

mod deferred;
mod exchange;
pub(in crate::quic) use deferred::Armed;
pub use deferred::{ClosedRegistration, ClosedReport, RevocationRegistration, RevocationReport};
pub use exchange::CloseOutcome;

const DRAIN_US: u64 = 250_000;
const TURN: Duration = Duration::from_millis(10);
const REPORT_BYTES: usize = if CLOSED_BYTES > REVOKED_BYTES {
    CLOSED_BYTES
} else {
    REVOKED_BYTES
};

// Only these two typed terminal records can acquire a detached socket. This is
// not a byte-oriented escape hatch for ordinary application writes after fencing.
enum Report {
    Revoked(Revoked),
    Closed(Closed),
}
impl Report {
    const fn byte_len(&self) -> usize {
        match self {
            Self::Revoked(_) => REVOKED_BYTES,
            Self::Closed(_) => CLOSED_BYTES,
        }
    }
    fn encode(self, binding: Binding, out: &mut [u8]) -> Result<usize, Error> {
        let limits = &ProtocolLimits::ABSOLUTE;
        let direction = InputDirection::HostToViewer;
        let delivery = InputDelivery::Reliable;
        match self {
            Self::Revoked(report) => {
                lease_revoked::encode(report, binding, limits, out, direction, delivery)
            }
            Self::Closed(report) => {
                closure::encode_closed(report, binding, limits, out, direction, delivery)
            }
        }
        .map_err(|_| Error::Malformed)
    }
}

struct Drain {
    native: NativeQuicUdpConnection,
    cx: Cx,
    gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    route: StreamRoute,
    binding: Binding,
    empty: EmptySendState,
    streams: usize,
    last: u64,
    until: u64,
    bytes: [u8; REPORT_BYTES],
    byte_len: usize,
}

impl QuicRecords {
    /// End this connection and attempt ONE content-free `LeaseRevoked` report.
    /// The caller must already have fenced the exact input lease. `cx` is the
    /// parent's cleanup context on the original timer driver, not a resurrected
    /// session context. The immutable ingress/destination guard still applies.
    ///
    /// The original owner is closed synchronously, even before the returned
    /// future is polled. Dropping that future releases its socket. No ordinary
    /// send/receive callback is invoked, and no new authority can be admitted.
    /// A foreign connection identity is rejected WITHOUT closing that connection.
    ///
    /// Unstaged application writes are discarded. A partially staged record,
    /// native pending/retransmission payload, or queued outbound datagram causes
    /// terminal `Backpressure`: the pinned native API cannot discard that work
    /// selectively, and flushing it would violate the fence. This result never
    /// authorizes a retry on the closed owner. Absence of a report is UNKNOWN.
    ///
    /// The fixed 250 ms budget starts HERE, not at the first poll or each retry.
    /// `Ok` means transport acknowledgement of this report, not peer application
    /// processing, native cleanup, or receipt accounting. Stages are unchanged.
    pub fn close_with_revocation(
        &mut self,
        cx: &Cx,
        original: &ConnectionBinding,
        route: StreamRoute,
        binding: Binding,
        report: Revoked,
    ) -> impl Future<Output = Result<(), Error>> + use<> {
        let prepared = if self.is_bound_to(original) {
            let prepared = if self.deferred_revocation.is_some() {
                Err(Error::InvalidPolicy)
            } else {
                self.prepare_revocation(cx, route, binding, report)
            };
            self.close();
            prepared
        } else {
            Err(Error::WrongRoute)
        };
        async move { prepared?.run().await }
    }

    /// Fence and enter ordered session teardown BEFORE calling this method.
    /// Attempt one typed `Closed` report on the original host control stream.
    /// Its cleanup/effect stages must come from the original owner; this method
    /// neither performs native cleanup nor converts absent receipts to zero.
    ///
    /// The same terminal-only drain as `close_with_revocation` closes ordinary
    /// I/O synchronously, discards unstaged writes, refuses retained native data,
    /// preserves the immutable security gate and starts its 250-ms budget now.
    /// A pre-registered lease-revocation report takes precedence: refuse this
    /// competing report rather than creating a second post-fence send path.
    /// `Ok` is a transport acknowledgement, not confirmation of remote cleanup.
    pub fn close_with_closed(
        &mut self,
        cx: &Cx,
        original: &ConnectionBinding,
        route: StreamRoute,
        binding: Binding,
        report: Closed,
    ) -> impl Future<Output = Result<(), Error>> + use<> {
        let prepared = if self.is_bound_to(original) {
            let prepared = if self.deferred_revocation.is_some() {
                Err(Error::InvalidPolicy)
            } else {
                self.prepare_report(cx, route, binding, Report::Closed(report))
            };
            self.close();
            prepared
        } else {
            Err(Error::WrongRoute)
        };
        async move { prepared?.run().await }
    }

    fn terminal_route(
        &self,
        route: StreamRoute,
        binding: Binding,
        minimum: usize,
    ) -> Result<(), Error> {
        let native = self.native.as_ref().ok_or(Error::Closed)?;
        if native.connection().role() != StreamRole::Server
            || native.connection().state() != QuicConnectionState::Established
            || !route.outbound
            || route.messages != Messages::SessionControl
            || route.priority != Priority::Critical
            || route.binding != binding.channel
            || route.maximum < minimum
            || !self.has_route(Route::Stream(route))
        {
            return Err(Error::WrongRoute);
        }
        Ok(())
    }

    fn prepare_revocation(
        &mut self,
        cx: &Cx,
        route: StreamRoute,
        binding: Binding,
        report: Revoked,
    ) -> Result<Drain, Error> {
        self.prepare_report(cx, route, binding, Report::Revoked(report))
    }

    fn prepare_report(
        &mut self,
        cx: &Cx,
        route: StreamRoute,
        binding: Binding,
        report: Report,
    ) -> Result<Drain, Error> {
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let started = now(cx)?;
        let until = started.checked_add(DRAIN_US).ok_or(Error::Clock)?;
        if self.last_now.is_some_and(|last| started < last) {
            return Err(Error::Clock);
        }
        if self
            .terminal_lifetime_check
            .as_ref()
            .is_some_and(|gate| !gate())
        {
            return Err(Error::Unauthorized);
        }
        self.terminal_route(route, binding, report.byte_len())?;
        let empty = self.terminal_sender(route)?;
        let mut bytes = [0; REPORT_BYTES];
        let byte_len = report.encode(binding, &mut bytes)?;
        Ok(Drain {
            native: self.native.take().ok_or(Error::Closed)?,
            cx: cx.clone(),
            gate: self.terminal_lifetime_check.clone(),
            route,
            binding,
            empty,
            streams: self.streams.len(),
            last: started,
            until,
            bytes,
            byte_len,
        })
    }
    fn terminal_sender(&mut self, route: StreamRoute) -> Result<EmptySendState, Error> {
        let native = self.native.as_ref().ok_or(Error::Closed)?;
        let streams = native.connection().inner().streams();
        if streams.len() > self.streams.len()
            || native
                .connection()
                .inner()
                .pending_outbound_datagram_count()
                != 0
            || self.pending_writes.iter().any(|write| write.offset != 0)
        {
            return Err(Error::Backpressure);
        }
        // Exact EMPTY witnesses cover private pending AND retransmission maps.
        // Packet counts, ACK counters and queue lengths do not prove absence.
        for sender in &mut self.senders {
            let live = streams
                .stream(sender.route.stream)
                .map_err(|_| Error::Native)?;
            if !sender.empty.matches(live) {
                return Err(Error::Backpressure);
            }
        }
        let empty = EmptySendState(
            self.senders
                .iter()
                .find(|sender| sender.route == route)
                .ok_or(Error::WrongRoute)?
                .empty
                .0
                .clone(),
        );
        Ok(empty)
    }
}

impl Drain {
    fn check(&mut self) -> Result<u64, Error> {
        self.cx.checkpoint().map_err(|_| Error::Cancelled)?;
        if self.gate.as_ref().is_some_and(|gate| !gate()) {
            return Err(Error::Unauthorized);
        }
        let at = now(&self.cx)?;
        if at < self.last {
            return Err(Error::Clock);
        }
        self.last = at;
        if at >= self.until {
            return Err(Error::Expired);
        }
        if self.native.connection().state() != QuicConnectionState::Established
            || self.native.connection().inner().streams().len() > self.streams
        {
            return Err(Error::Closed);
        }
        Ok(at)
    }

    async fn run(mut self) -> Result<(), Error> {
        let at = self.check()?;
        let cx = self.cx.clone();
        let remaining = Duration::from_micros(self.until - at);
        timeout(cx.now(), remaining, self.run_inner())
            .await
            .map_err(|_| Error::Expired)?
    }

    async fn run_inner(&mut self) -> Result<(), Error> {
        let mut queued = false;
        loop {
            let at = self.check()?;
            let streams = self.native.connection().inner().streams();
            let live = streams
                .stream(self.route.stream)
                .map_err(|_| Error::Native)?;
            if queued && self.empty.matches(live) {
                return Ok(());
            }
            if !queued
                && streams.connection_send_remaining() >= self.byte_len as u64
                && self
                    .native
                    .connection()
                    .inner()
                    .stream_send_credit_remaining(self.route.stream)
                    >= self.byte_len as u64
            {
                // The one fixed record is smaller than the native protected
                // packet allowance. No FIN: it could hide the terminal record
                // behind the viewer's independent stream-closure fence.
                self.native
                    .connection_mut()
                    .write_stream(
                        &self.cx,
                        self.route.stream,
                        Bytes::copy_from_slice(&self.bytes[..self.byte_len]),
                        false,
                    )
                    .map_err(|_| Error::Native)?;
                queued = true;
            }
            let wait = TURN.min(Duration::from_micros(self.until - at));
            super::poll_io(
                &self.cx,
                self.gate.as_deref(),
                &mut || true,
                at,
                Some(self.until),
                self.native.drive_io_once(&self.cx, wait),
            )
            .await?;
        }
    }
}
