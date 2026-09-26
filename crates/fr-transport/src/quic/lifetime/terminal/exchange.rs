//! One closing client exchange, never an ordinary post-close transport loan.
use super::{DRAIN_US, TURN};
use crate::quic::{
    ConnectionBinding, ControlRoutes, EmptySendState, Error, Inbound, Messages, Priority,
    QuicRecords, Route, now, validate_record,
};
use asupersync::{
    bytes::Bytes,
    cx::Cx,
    net::quic_native::{NativeQuicUdpConnection, QuicConnectionState, StreamRole},
    time::timeout,
};
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    Kind,
    authority::Binding,
    closure::{self, CloseRequest, Closed, REQUEST_BYTES},
    input::{InputDelivery, InputDirection},
};
use std::{future::Future, sync::Arc, time::Duration};

/// Independent stages of the original closing exchange. An acknowledged request
/// is NOT a host application acknowledgement. A received report is retained even
/// if sending its transport ACK fails; its cleanup/effects are never upgraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CloseOutcome {
    pub request_acknowledged: bool,
    pub report: Option<Closed>,
    pub transport: Result<(), Error>,
}
impl CloseOutcome {
    fn initial(transport: Result<(), Error>) -> Self {
        Self {
            request_acknowledged: false,
            report: None,
            transport,
        }
    }
}

struct Exchange {
    native: NativeQuicUdpConnection,
    cx: Cx,
    gate: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
    outbound: crate::quic::StreamRoute,
    incoming: Inbound,
    binding: Binding,
    empty: EmptySendState,
    streams: usize,
    last: u64,
    until: u64,
    bytes: [u8; REQUEST_BYTES],
    queued: bool,
    remaining_records: u8,
    outcome: CloseOutcome,
}
impl QuicRecords {
    /// Close ordinary I/O at CALL time, then exchange ONE session `CloseRequest`
    /// for the original host's Closed report. The caller first stops application
    /// observation/input admission. This API performs no native-input cleanup.
    ///
    /// Existing outbound native/retransmission data, partial writes and queued
    /// datagrams refuse rather than flushing stale work. Unstaged writes are
    /// discarded. The original inbound control parser/remainder is TRANSFERRED,
    /// not restarted mid-record. Other lanes are never delivered to applications.
    /// At most 32 control records are examined; the configured receive bounds are not raised.
    ///
    /// The immutable security gate and context cancellation remain enforced.
    /// `until_micros` can only shorten the 250-ms construction-time budget. No
    /// heartbeat, queued frame, retry or delayed first poll refreshes that budget.
    /// A foreign connection proof is non-mutating. Every matched attempt closes,
    /// including refusal or an unpolled dropped future. This is not a reconnect.
    #[allow(clippy::too_many_arguments)]
    pub fn close_with_request(
        &mut self,
        cx: &Cx,
        original: &ConnectionBinding,
        routes: ControlRoutes,
        binding: Binding,
        request: CloseRequest,
        until_micros: u64,
    ) -> impl Future<Output = CloseOutcome> + use<> {
        let prepared = if self.is_bound_to(original) {
            let prepared = self.prepare_close_exchange(cx, routes, binding, request, until_micros);
            self.close();
            prepared
        } else {
            Err(Error::WrongRoute)
        };
        async move {
            match prepared {
                Ok(exchange) => exchange.run().await,
                Err(error) => CloseOutcome::initial(Err(error)),
            }
        }
    }

    fn prepare_close_exchange(
        &mut self,
        cx: &Cx,
        routes: ControlRoutes,
        binding: Binding,
        request: CloseRequest,
        until_micros: u64,
    ) -> Result<Exchange, Error> {
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let started = now(cx)?;
        let until = started
            .checked_add(DRAIN_US)
            .ok_or(Error::Clock)?
            .min(until_micros);
        if self.last_now.is_some_and(|last| started < last) {
            return Err(Error::Clock);
        }
        if started >= until {
            return Err(Error::Expired);
        }
        if self
            .terminal_lifetime_check
            .as_ref()
            .is_some_and(|gate| !gate())
        {
            return Err(Error::Unauthorized);
        }
        if self.deferred_revocation.is_some() {
            return Err(Error::InvalidPolicy);
        }
        let native = self.native.as_ref().ok_or(Error::Closed)?;
        if native.connection().role() != StreamRole::Client
            || native.connection().state() != QuicConnectionState::Established
            || routes.inbound.outbound
            || !routes.outbound.outbound
            || routes.inbound.stream == routes.outbound.stream
            || [routes.inbound, routes.outbound].iter().any(|r| {
                r.messages != Messages::SessionControl
                    || r.priority != Priority::Critical
                    || r.binding != binding.channel
                    || r.maximum < closure::CLOSED_BYTES.max(REQUEST_BYTES)
                    || !self.has_route(Route::Stream(*r))
            })
        {
            return Err(Error::WrongRoute);
        }
        let mut bytes = [0; REQUEST_BYTES];
        closure::encode_request(
            request,
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(|_| Error::Malformed)?;
        let empty = self.terminal_sender(routes.outbound)?;
        let index = self
            .inbound
            .iter()
            .position(|s| s.route == routes.inbound)
            .ok_or(Error::WrongRoute)?;
        self.inbound[index].framing.tick(started)?;
        if self.inbound[index].fin {
            return Err(Error::Closed);
        }
        Ok(Exchange {
            native: self.native.take().ok_or(Error::Closed)?,
            cx: cx.clone(),
            gate: self.terminal_lifetime_check.clone(),
            outbound: routes.outbound,
            incoming: self.inbound.swap_remove(index),
            binding,
            empty,
            streams: self.streams.len(),
            last: started,
            until,
            bytes,
            queued: false,
            remaining_records: 32,
            outcome: CloseOutcome::initial(Ok(())),
        })
    }
}
impl Exchange {
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
    async fn run(mut self) -> CloseOutcome {
        self.outcome.transport = match self.check() {
            Err(error) => Err(error),
            Ok(at) => {
                let remaining = Duration::from_micros(self.until - at);
                timeout(self.cx.now(), remaining, self.run_inner())
                    .await
                    .unwrap_or(Err(Error::Expired))
            }
        };
        self.outcome
    }
    fn offer(&mut self) -> Result<(), Error> {
        let streams = self.native.connection().inner().streams();
        if self.queued {
            let live = streams
                .stream(self.outbound.stream)
                .map_err(|_| Error::Native)?;
            self.outcome.request_acknowledged |= self.empty.matches(live);
        } else if streams.connection_send_remaining() >= REQUEST_BYTES as u64
            && self
                .native
                .connection()
                .inner()
                .stream_send_credit_remaining(self.outbound.stream)
                >= REQUEST_BYTES as u64
        {
            self.native
                .connection_mut()
                .write_stream(
                    &self.cx,
                    self.outbound.stream,
                    Bytes::copy_from_slice(&self.bytes),
                    false,
                )
                .map_err(|_| Error::Native)?;
            self.queued = true;
        }
        Ok(())
    }
    fn receive(&mut self, at: u64) -> Result<bool, Error> {
        // Cooperative bounded work even if the peer floods valid nonterminal
        // control records. Preserve framing and its original partial deadline.
        for _ in 0..16 {
            if self.remaining_records == 0 {
                return Err(Error::TooLarge);
            }
            let s = &mut self.incoming;
            if s.framing.frame(at)?.is_none() {
                if s.remainder.is_empty() {
                    s.remainder = self
                        .native
                        .connection_mut()
                        .read_stream(&self.cx, s.route.stream, s.route.maximum)
                        .map_err(|_| Error::Native)?;
                }
                let count = s.framing.push(&s.remainder, at)?;
                s.remainder = s.remainder.slice(count..);
            }
            if let Some(bytes) = s.framing.frame(at)? {
                validate_record(bytes, s.route.maximum, s.route.binding, s.route.messages)?;
                self.remaining_records -= 1;
                if u16::from_be_bytes([bytes[6], bytes[7]]) == Kind::Closed as u16 {
                    self.outcome.report = Some(
                        closure::decode_closed(
                            bytes,
                            self.binding,
                            &ProtocolLimits::ABSOLUTE,
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable,
                        )
                        .map_err(|_| Error::Malformed)?,
                    );
                    return Ok(true); // Never interpret a record after Closed.
                }
                // No callback, acknowledgement or semantic action for old
                // control records, including challenges and media attachments.
                s.framing.consume(at)?;
            } else if s.remainder.is_empty() {
                if self
                    .native
                    .connection()
                    .is_stream_eof(s.route.stream)
                    .map_err(|_| Error::Native)?
                {
                    s.framing.finish(at)?;
                    return Err(Error::Closed);
                }
                return Ok(false);
            }
        }
        Ok(false)
    }
    async fn run_inner(&mut self) -> Result<(), Error> {
        loop {
            let at = self.check()?;
            self.offer()?;
            if self.receive(at)? {
                // Preserve the report even if this bounded ACK flush fails.
                // No subsequent application reads or writes can occur.
                return super::super::poll_io(
                    &self.cx,
                    self.gate.as_deref(),
                    &mut || true,
                    at,
                    Some(self.until),
                    self.native.flush(&self.cx),
                )
                .await
                .map(|_| ());
            }
            let wait = TURN.min(Duration::from_micros(self.until - at));
            super::super::poll_io(
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
