//! One bounded native control pair, transitioned in place after negotiation.
//! No second socket, retransmission engine, or application authentication mode.
use super::{
    ConnectionBinding, Cx, Error, HEADER_BYTES, Messages, NativeQuicUdpConnection, Policy,
    Priority, QuicRecords, RecordStream, Route, StreamId, StreamRole, StreamRoute,
};
use std::net::SocketAddr;

/// Actual transport routes. No stream number or binding supplied by the peer
/// can attach itself to these owners.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlRoutes {
    pub outbound: StreamRoute,
    pub inbound: StreamRoute,
}
impl QuicRecords {
    /// Claim clock exchange state once for this actual connection, including
    /// after an earlier attachment is dropped. Reusing sequence one on the same
    /// connection could accept a delayed reply as a new measurement.
    pub fn claim_clock(&mut self, routes: ControlRoutes) -> Result<ConnectionBinding, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if self.clock_attached
            || routes.inbound.outbound
            || !routes.outbound.outbound
            || routes.inbound.binding == 0
            || routes.inbound.binding != routes.outbound.binding
            || [routes.inbound, routes.outbound].iter().any(|r| {
                r.messages != Messages::SessionControl
                    || r.priority != Priority::Critical
                    || r.maximum < fr_wire::clock::REPLY_BYTES
                    || !self.has_route(Route::Stream(*r))
            })
            || self.receive_ended(routes.inbound)?
        {
            return Err(Error::WrongRoute);
        }
        self.clock_attached = true;
        Ok(self.binding())
    }
    /// Reserve the one initial display-choice exchange on this control pair.
    /// Destruction never permits replay of another catalog on the same session.
    pub fn claim_display_selection(
        &mut self,
        routes: ControlRoutes,
    ) -> Result<ConnectionBinding, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if self.display_selection_claimed
            || routes.inbound.outbound
            || !routes.outbound.outbound
            || routes.inbound.binding == 0
            || routes.inbound.binding != routes.outbound.binding
            || [routes.inbound, routes.outbound].iter().any(|r| {
                r.messages != Messages::SessionControl
                    || r.priority != Priority::Critical
                    || r.maximum < fr_wire::display::SELECT_BYTES
                    || !self.has_route(Route::Stream(*r))
            })
            || self.receive_ended(routes.inbound)?
        {
            return Err(Error::WrongRoute);
        }
        self.display_selection_claimed = true;
        Ok(self.binding())
    }
    /// Adopt a fresh, TLS-established native connection for startup only.
    /// The listener must already enforce tailnet ingress. The session owner
    /// obtains `LocalAPI` admission before sending or consuming application data.
    pub fn bootstrap(
        mut native: NativeQuicUdpConnection,
        cx: &Cx,
        policy: Policy,
    ) -> Result<(Self, ControlRoutes), Error> {
        if !native.connection().inner().streams().is_empty() {
            return Err(Error::InvalidPolicy);
        }
        let role = native.connection().role();
        let outgoing = native
            .connection_mut()
            .open_uni_stream(cx)
            .map_err(|_| Error::Native)?;
        let expected = StreamId(if role == StreamRole::Client { 2 } else { 3 });
        if outgoing != expected {
            return Err(Error::WrongRoute);
        }
        let maximum = fr_wire::negotiation::MAX_RECORD;
        let outbound = StreamRoute {
            stream: outgoing,
            binding: 0,
            messages: Messages::Negotiation,
            priority: Priority::Critical,
            outbound: true,
            maximum,
        };
        let inbound = StreamRoute {
            stream: StreamId(if role == StreamRole::Client { 3 } else { 2 }),
            outbound: false,
            ..outbound
        };
        let routes = ControlRoutes { outbound, inbound };
        Ok((
            Self::new(native, cx, &[outbound, inbound], &[], policy)?,
            routes,
        ))
    }

    /// Kernel/transport endpoints, never header-provided identity assertions.
    pub fn addresses(&self) -> Result<(SocketAddr, SocketAddr), Error> {
        let native = self.native.as_ref().ok_or(Error::Closed)?;
        Ok((native.local_addr(), native.peer_addr()))
    }
    pub fn role(&self) -> Result<StreamRole, Error> {
        Ok(self
            .native
            .as_ref()
            .ok_or(Error::Closed)?
            .connection()
            .role())
    }
    /// All application prefixes on this stream have entered native ownership.
    /// This is NOT an acknowledgement of peer receipt or an external effect.
    pub fn send_staged(&self, route: StreamRoute) -> Result<bool, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if !route.outbound || !self.streams.contains(&route) {
            return Err(Error::WrongRoute);
        }
        Ok(!self.pending_writes.iter().any(|p| p.route == route))
    }
    /// Install the initial control binding once, on the SAME stream pair.
    /// Old sends must already be staged; retransmission reservations/deadlines
    /// survive. A partial inbound record cannot be relabeled or gain time.
    /// Unread transport bytes keep their original bytes and must pass the new
    /// binding check; pipelined zero-bound records are not reinterpreted.
    pub fn bind_control(
        &mut self,
        cx: &Cx,
        routes: ControlRoutes,
        binding: u32,
        maximum: usize,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<ControlRoutes, Error> {
        self.check(cx, &mut authorize)?;
        if binding == 0
            || !(HEADER_BYTES..=routes.inbound.maximum).contains(&maximum)
            || routes.inbound.messages != Messages::Negotiation
            || routes.outbound.messages != Messages::Negotiation
            || routes.inbound.binding != 0
            || routes.outbound.binding != 0
            || !routes.outbound.outbound
            || routes.inbound.outbound
            || self.streams.len() != 2
            || !self.has_route(Route::Stream(routes.inbound))
            || !self.has_route(Route::Stream(routes.outbound))
        {
            return Err(Error::WrongRoute);
        }
        if !self.send_staged(routes.outbound)? {
            return Err(Error::Backpressure);
        }
        let receiving = self
            .inbound
            .iter()
            .position(|s| s.route == routes.inbound)
            .ok_or(Error::WrongRoute)?;
        if self.inbound[receiving].fin || self.inbound[receiving].framing.buffered_bytes() != 0 {
            return Err(Error::Backpressure);
        }
        let framing = RecordStream::new(maximum, binding, self.policy.record_lifetime_micros)?;
        let next = ControlRoutes {
            outbound: StreamRoute {
                binding,
                maximum,
                messages: Messages::SessionControl,
                ..routes.outbound
            },
            inbound: StreamRoute {
                binding,
                maximum,
                messages: Messages::SessionControl,
                ..routes.inbound
            },
        };
        self.check(cx, &mut authorize)?;
        for route in &mut self.streams {
            *route = if route.outbound {
                next.outbound
            } else {
                next.inbound
            };
        }
        self.senders[0].route = next.outbound;
        self.inbound[receiving].route = next.inbound;
        self.inbound[receiving].framing = framing;
        Ok(next)
    }
}
