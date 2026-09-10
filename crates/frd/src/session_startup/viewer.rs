//! Production native viewer startup and persistent observation control. The
//! caller supplies a TLS-established QUIC connection and a session-local Cx.
//! No windows, decoder, input grant, peer identity or alternative transport are
//! invented here. Application dispatch is synchronous and bounded by `QuicRecords`.
use super::{Error, now};
use asupersync::{cx::Cx, net::quic_native::NativeQuicUdpConnection, types::CancelKind};
use fr_client::{
    authority::ObservationResponder,
    input::ClientInstant,
    startup::{ApprovalNotice, Opened, Startup},
};
use fr_transport::quic::{
    self, ConnectionBinding, ControlRoutes, Disposition, Policy, QuicRecords, Route,
};
use fr_wire::{
    Kind,
    authority::{self, Message, Scope},
    input::{InputDelivery, InputDirection},
    negotiation::{self, Offer},
};
use std::{cell::Cell, fmt, future::Future, time::Duration};

// Local responsiveness bound only. Host deadlines are opaque and are never
// compared with this clock or translated into new observation/input authority.
const SILENCE_US: u64 = 3_000_000;

/// Drives the existing shared Startup over the SAME native control pair. One
/// exact record survives backpressure. Drop, cancellation and uncertainty close
/// this session rather than restarting a partly sent negotiation.
pub struct Viewer {
    cx: Cx,
    startup: Option<Startup>,
    transport: Option<QuicRecords>,
    routes: ControlRoutes,
    detached: bool,
}
impl fmt::Debug for Viewer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ViewerStartup")
            .field("startup", &self.startup)
            .finish_non_exhaustive()
    }
}
impl Viewer {
    /// TLS/hostname/ALPN verification has already completed in Asupersync. The
    /// chosen endpoint must come from the caller's qualified tailnet connection
    /// path. This does not accept a plaintext socket or enable skip-verification.
    /// `cx` MUST be dedicated to this viewer, not a process-wide context.
    pub fn new(
        cx: Cx,
        native: NativeQuicUdpConnection,
        offer: Offer,
        policy: Policy,
        timeout: Duration,
    ) -> Result<Self, Error> {
        if native.connection().role() != asupersync::net::quic_native::StreamRole::Client {
            return Err(Error::Order);
        }
        let timeout =
            u64::try_from(timeout.as_micros()).map_err(|_| Error::InvalidConfiguration)?;
        let startup = Startup::new(offer, now(&cx)?, timeout).map_err(Error::ClientStartup)?;
        let (transport, routes) = QuicRecords::bootstrap(native, &cx, policy)?;
        Ok(Self {
            cx,
            startup: Some(startup),
            transport: Some(transport),
            routes,
            detached: false,
        })
    }
    pub fn approval(&self) -> Option<ApprovalNotice> {
        self.startup.as_ref().and_then(Startup::approval)
    }
    pub fn is_complete(&self) -> bool {
        self.startup.as_ref().is_some_and(Startup::is_complete)
    }
    fn step(&mut self) -> Result<(), Error> {
        let current = now(&self.cx)?;
        let startup = self.startup.as_mut().ok_or(Error::Closed)?;
        let transport = self.transport.as_mut().ok_or(Error::Closed)?;
        startup.tick(current).map_err(Error::ClientStartup)?;
        if transport.receive_ended(self.routes.inbound)? {
            return Err(Error::Closed);
        }
        if startup.is_complete() {
            return Ok(());
        }
        if let Some((binding, maximum)) = startup
            .binding_to_install(current)
            .map_err(Error::ClientStartup)?
        {
            match transport.bind_control(&self.cx, self.routes, binding.id, maximum, || {
                self.cx.checkpoint().is_ok()
            }) {
                Ok(routes) => {
                    self.routes = routes;
                    startup
                        .bound(binding.id, now(&self.cx)?)
                        .map_err(Error::ClientStartup)?;
                }
                Err(quic::Error::Backpressure) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        let until = startup.deadline_us();
        if let Some(bytes) = startup
            .pending(now(&self.cx)?)
            .map_err(Error::ClientStartup)?
        {
            match transport.send(
                &self.cx,
                Route::Stream(self.routes.outbound),
                bytes,
                until,
                || now(&self.cx).is_ok_and(|n| n < until),
            ) {
                Ok(()) => startup.sent(now(&self.cx)?).map_err(Error::ClientStartup)?,
                Err(quic::Error::Backpressure) => return Ok(()),
                Err(e) => return Err(e.into()),
            }
        }
        if startup.is_complete() {
            return Ok(());
        }
        let consumed = Cell::new(false);
        let mut bytes = [0; negotiation::MAX_RECORD];
        let mut len = 0;
        transport.receive_ready(
            &self.cx,
            || now(&self.cx).is_ok_and(|n| n < until),
            |_| !consumed.get(),
            |route, record| {
                if route != Route::Stream(self.routes.inbound) || record.len() > bytes.len() {
                    return Err(());
                }
                bytes[..record.len()].copy_from_slice(record);
                len = record.len();
                consumed.set(true);
                Ok(Disposition::Consumed)
            },
        )?;
        if consumed.get() {
            startup
                .receive(&bytes[..len], now(&self.cx)?)
                .map_err(Error::ClientStartup)?;
        }
        startup.tick(now(&self.cx)?).map_err(Error::ClientStartup)
    }
    pub fn tick(&mut self) -> Result<(), Error> {
        let result = self.step();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Guard is installed at CALL time: even dropping an unpolled drive is an
    /// explicit session cancellation, not permission to replay pending startup.
    pub fn drive(&mut self, wait: Duration) -> impl Future<Output = Result<(), Error>> + '_ {
        let guard = StartupDrive {
            viewer: self,
            complete: false,
        };
        async move {
            let mut guard = guard;
            guard.viewer.tick()?;
            if wait > Duration::from_millis(100) {
                return Err(Error::InvalidConfiguration);
            }
            // Even after local completion, BindingAccepted may still be a
            // retained write. Continue pumping the same connection until the
            // caller transfers it to ViewerSession.
            {
                let until = guard
                    .viewer
                    .startup
                    .as_ref()
                    .ok_or(Error::Closed)?
                    .deadline_us();
                let remaining = until
                    .checked_sub(now(&guard.viewer.cx)?)
                    .ok_or(Error::Expired)?;
                let cx = &guard.viewer.cx;
                guard
                    .viewer
                    .transport
                    .as_mut()
                    .ok_or(Error::Closed)?
                    .drive(cx, wait.min(Duration::from_micros(remaining)), || {
                        now(cx).is_ok_and(|n| n < until)
                    })
                    .await?;
                guard.viewer.tick()?;
            }
            guard.complete = true;
            Ok(())
        }
    }
    /// Transfer the SAME connection and its retained sends into steady-state
    /// control. `BindingAccepted` has entered transport ownership; it is not a
    /// host acknowledgement, decoded frame, visible desktop or input grant.
    pub fn finish(mut self) -> Result<ViewerSession, Error> {
        self.tick()?;
        if !self.is_complete() {
            return Err(Error::Order);
        }
        let opened = self
            .startup
            .take()
            .ok_or(Error::Closed)?
            .finish(now(&self.cx)?)
            .map_err(Error::ClientStartup)?;
        let transport = self.transport.take().ok_or(Error::Closed)?;
        let session = ViewerSession::new(self.cx.clone(), transport, self.routes, opened)?;
        self.detached = true;
        Ok(session)
    }
    pub fn close(&mut self) {
        if self.detached {
            return;
        }
        if let Some(startup) = &mut self.startup {
            startup.close();
        }
        if let Some(transport) = &mut self.transport {
            transport.close();
        }
        self.cx.cancel_fast(CancelKind::User);
    }
}
impl Drop for Viewer {
    fn drop(&mut self) {
        self.close();
    }
}
struct StartupDrive<'a> {
    viewer: &'a mut Viewer,
    complete: bool,
}
impl Drop for StartupDrive<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.viewer.close();
        }
    }
}

/// The live native viewer control loop. Valid observation challenges are handled
/// here; all other records remain with the caller's bounded application handler.
/// There is no implicit media, input, clock or capability acknowledgement.
pub struct ViewerSession {
    cx: Cx,
    transport: QuicRecords,
    connection: ConnectionBinding,
    routes: ControlRoutes,
    opened: Opened,
    responder: ObservationResponder,
    last: u64,
    heard_until: u64,
    closed: bool,
}
impl ViewerSession {
    fn new(
        cx: Cx,
        transport: QuicRecords,
        routes: ControlRoutes,
        opened: Opened,
    ) -> Result<Self, Error> {
        let current = now(&cx)?;
        let responder = ObservationResponder::new(
            authority::Binding {
                channel: opened.binding.id,
                session: opened.binding.remote_session,
            },
            opened.selection.limits,
            ClientInstant(current),
        )
        .map_err(Error::ClientRenewal)?;
        let heard_until = current.checked_add(SILENCE_US).ok_or(Error::Clock)?;
        let connection = transport.binding();
        let mut this = Self {
            cx,
            transport,
            connection,
            routes,
            opened,
            responder,
            last: current,
            heard_until,
            closed: false,
        };
        this.check()?;
        Ok(this)
    }
    pub fn metadata(&self) -> &Opened {
        &self.opened
    }
    pub fn is_closed(&self) -> bool {
        self.closed || self.transport.is_closed()
    }
    pub fn check(&mut self) -> Result<(), Error> {
        let result = (|| {
            if self.closed
                || !self.transport.is_bound_to(&self.connection)
                || self.transport.is_closed()
                || self.transport.receive_ended(self.routes.inbound)?
            {
                return Err(Error::Closed);
            }
            let current = now(&self.cx)?;
            if current < self.last {
                return Err(Error::Clock);
            }
            if current >= self.heard_until {
                return Err(Error::Expired);
            }
            self.last = current;
            self.responder
                .tick(ClientInstant(current))
                .map_err(Error::ClientRenewal)
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Borrow for already-bound media/input/clock owners. Replacement is refused
    /// at the next session check. The session-local Cx must also own their work.
    pub fn io(&mut self) -> Result<(&mut QuicRecords, ControlRoutes), Error> {
        self.check()?;
        Ok((&mut self.transport, self.routes))
    }
    fn send_response(&mut self) -> Result<(), Error> {
        self.check()?;
        let Some(deadline) = self.responder.response_deadline() else {
            return Ok(());
        };
        let until = deadline.0.min(self.heard_until);
        if let Some(bytes) = self
            .responder
            .pending(ClientInstant(now(&self.cx)?))
            .map_err(Error::ClientRenewal)?
        {
            match self.transport.send(
                &self.cx,
                Route::Stream(self.routes.outbound),
                bytes,
                until,
                || now(&self.cx).is_ok_and(|n| n < until),
            ) {
                Ok(()) => self
                    .responder
                    .sent(ClientInstant(now(&self.cx)?))
                    .map_err(Error::ClientRenewal)?,
                Err(quic::Error::Backpressure) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
    fn step(
        &mut self,
        other: &mut impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        self.send_response()?;
        let binding = authority::Binding {
            channel: self.opened.binding.id,
            session: self.opened.binding.remote_session,
        };
        let limits = self.opened.selection.limits;
        let cx = &self.cx;
        let inbound = self.routes.inbound;
        let responder = &mut self.responder;
        let heard_until = &mut self.heard_until;
        let old_until = *heard_until;
        let mut failure = None;
        self.transport
            .receive(
                cx,
                || now(cx).is_ok_and(|n| n < old_until),
                |route, bytes| {
                    if route == Route::Stream(inbound)
                        && bytes.get(6..8) == Some(&(Kind::Challenge as u16).to_be_bytes())
                    {
                        let message = authority::decode(
                            bytes,
                            binding,
                            &limits,
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable,
                        );
                        match message {
                            Ok(Message::Challenge {
                                scope: Scope::Observation,
                                ..
                            }) => {
                                let n = now(cx).map_err(|e| {
                                    failure = Some(e);
                                })?;
                                match responder.accept(bytes, ClientInstant(n)) {
                                    Ok(()) => {
                                        *heard_until =
                                            n.checked_add(SILENCE_US).ok_or_else(|| {
                                                failure = Some(Error::Clock);
                                            })?;
                                        return Ok(Disposition::Consumed);
                                    }
                                    Err(fr_client::authority::Error::Backpressure) => {
                                        return Ok(Disposition::Blocked);
                                    }
                                    Err(e) => {
                                        failure = Some(Error::ClientRenewal(e));
                                        return Err(());
                                    }
                                }
                            }
                            Ok(_) => {} // A control challenge belongs to its distinct lease owner.
                            Err(e) => {
                                failure = Some(Error::ClientRenewal(
                                    fr_client::authority::Error::Wire(e),
                                ));
                                return Err(());
                            }
                        }
                    }
                    other(route, bytes)
                },
            )
            .map_err(|e| failure.unwrap_or(Error::Transport(e)))?;
        self.send_response()?;
        self.check()
    }
    pub fn tick(
        &mut self,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<(), Error> {
        let result = self.step(&mut other);
        if result.is_err() {
            self.close();
        }
        result
    }
    pub fn drive<'a>(
        &'a mut self,
        wait: Duration,
        mut other: impl FnMut(Route, &[u8]) -> Result<Disposition, ()> + 'a,
    ) -> impl Future<Output = Result<(), Error>> + 'a {
        let guard = SessionDrive {
            viewer: self,
            complete: false,
        };
        async move {
            let mut guard = guard;
            if wait > Duration::from_millis(100) {
                return Err(Error::InvalidConfiguration);
            }
            guard.viewer.step(&mut other)?;
            let until = guard
                .viewer
                .responder
                .response_deadline()
                .map_or(guard.viewer.heard_until, |d| {
                    d.0.min(guard.viewer.heard_until)
                });
            let cx = &guard.viewer.cx;
            let remaining = until.checked_sub(now(cx)?).ok_or(Error::Expired)?;
            guard
                .viewer
                .transport
                .drive(cx, wait.min(Duration::from_micros(remaining)), || {
                    now(cx).is_ok_and(|n| n < until)
                })
                .await?;
            guard.viewer.step(&mut other)?;
            guard.complete = true;
            Ok(())
        }
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.responder.stop();
        self.transport.close();
        self.cx.cancel_fast(CancelKind::User);
    }
}
impl Drop for ViewerSession {
    fn drop(&mut self) {
        self.close();
    }
}
struct SessionDrive<'a> {
    viewer: &'a mut ViewerSession,
    complete: bool,
}
impl Drop for SessionDrive<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.viewer.close();
        }
    }
}

#[cfg(test)]
mod tests;
