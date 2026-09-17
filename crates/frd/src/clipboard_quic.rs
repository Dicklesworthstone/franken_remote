//! Original-controller clipboard attachment joined to an off-network OS worker.
//! The running session retains its admission and drives the existing connection.
//! This module creates no listener, replacement lease, or synthetic viewer clock.
use asupersync::cx::Cx;
use fr_client::{
    clipboard::{ControllerClipboard, ControllerSynchronizer, ControllerTransport},
    input::{ClientInstant, InputClient},
};
use fr_core::{
    clipboard::{ClipboardSwitch, PlatformError},
    input_submission::InputSession,
    time::HostInstant,
};
use fr_transport::quic::{
    self, ConnectionBinding, Disposition, QuicRecords, clipboard::ClipboardChannel,
};
use fr_wire::clipboard::{
    Role,
    session::{
        Admission, ChannelSession, RecordSink, SessionError, TransportFailure,
        egress::{Egress, Transport},
        synchronize::{IdentifierFailure, NativeClipboard, Received, Synchronizer},
    },
};
use std::{sync::Arc, time::Duration};
mod shared;
mod worker;
use shared::{Clock, Outgoing, Shared, Turn};
pub use worker::{Worker, WorkerControl, WorkerSeed, WorkerTask};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    WrongConnection,
    WrongRole,
    Closed,
    Clock,
    Cancelled,
    SwitchChanged,
    Limit,
    Allocation,
    Poisoned,
    Thread,
    Native,
    Panicked,
    HandoffExpired,
    NativeSetup(PlatformError),
    Session(SessionError),
    Controller(fr_client::clipboard::Error),
    Presentation(fr_client::input::presentation::Error),
    AlreadyAttached,
    NotNegotiated,
    SetupExpired,
    ConsentRequired,
    Transport(quic::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Retired,
}

// A fixed batch avoids one RTT of head-of-line delay per 16-KiB chunk. Every
// retained native record keeps its OWN immutable permit/deadline until the
// whole native lane drains. The worker still has only one handoff slot.
const MAX_INFLIGHT_RECORDS: usize = 4;

/// At most four native in-flight permits, one outbound handoff record, and one
/// incoming queued/executing/deferred/uncollected record. Other lanes retain
/// their original transport budgets. No payload history or unbounded queue.
pub struct Bridge {
    lane: ClipboardChannel,
    connection: ConnectionBinding,
    shared: Arc<Shared>,
    clock: Clock,
    cx: Cx,
    switches: (u64, u64),
    pending: Option<Outgoing>,
    inflight: [Option<(Egress, u64)>; MAX_INFLIGHT_RECORDS],
    release_outbox: bool,
    retired: bool,
    reason: Option<Error>,
}
impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClipboardBridge")
            .field("retired", &self.retired)
            .finish_non_exhaustive()
    }
}
impl Bridge {
    /// Use the original input owner and its actual host timer domain. `granted`
    /// means separately negotiated/local clipboard approval, not control alone.
    pub fn host(
        cx: Cx,
        q: &QuicRecords,
        lane: ClipboardChannel,
        input: &InputSession,
        granted: bool,
    ) -> Result<(Self, WorkerSeed), Error> {
        Self::host_monitor(
            cx,
            q,
            lane,
            fr_core::clipboard::authority::Monitor::from_input(input),
            granted,
        )
    }
    pub(crate) fn host_monitor(
        cx: Cx,
        q: &QuicRecords,
        lane: ClipboardChannel,
        monitor: fr_core::clipboard::authority::Monitor,
        granted: bool,
    ) -> Result<(Self, WorkerSeed), Error> {
        lane.check(q).map_err(Error::Transport)?;
        if lane.outgoing().sender != Role::Host {
            return Err(Error::WrongRole);
        }
        let session = ChannelSession::with_monitor(
            monitor,
            lane.outgoing(),
            lane.limits(),
            granted,
            HostInstant::from_micros(shared::now(&cx)?),
        )
        .map_err(Error::Session)?;
        let transport = session.transport();
        Self::join(
            cx,
            q,
            lane,
            worker::Session::Host(session),
            transport.clone(),
            Clock::Host(transport.clone()),
            Clock::Host(transport),
        )
    }
    /// Join the actual accepted viewer grant and completed route. Authority and
    /// presentation remain tied to the ORIGINAL `InputClient`; no host owner exists.
    pub fn controller(
        cx: Cx,
        q: &QuicRecords,
        lane: ClipboardChannel,
        input: &mut InputClient,
        granted: bool,
    ) -> Result<(Self, WorkerSeed), Error> {
        lane.check(q).map_err(Error::Transport)?;
        if lane.outgoing().sender != Role::Controller {
            return Err(Error::WrongRole);
        }
        let session = input
            .attach_clipboard_lane(
                lane.parent(),
                lane.outgoing(),
                lane.limits(),
                granted,
                ClientInstant(shared::now(&cx)?),
            )
            .map_err(Error::Controller)?;
        let network = session.transport();
        let worker = session.transport();
        Self::join(
            cx,
            q,
            lane,
            worker::Session::Controller(session),
            network.transport(),
            Clock::Controller(network),
            Clock::Controller(worker),
        )
    }
    /// Attach the running viewer's real decoder-backed input owner. This keeps
    /// the original visibility/receiver witness; no synthetic input is created.
    pub fn presented(
        cx: Cx,
        q: &QuicRecords,
        lane: ClipboardChannel,
        input: &mut fr_client::input::presentation::PresentedInput,
        granted: bool,
    ) -> Result<(Self, WorkerSeed), Error> {
        lane.check(q).map_err(Error::Transport)?;
        if lane.outgoing().sender != Role::Controller {
            return Err(Error::WrongRole);
        }
        let session = input
            .attach_clipboard_lane(
                lane.parent(),
                lane.outgoing(),
                lane.limits(),
                granted,
                ClientInstant(shared::now(&cx)?),
            )
            .map_err(Error::Presentation)?;
        let network = session.transport();
        let worker = session.transport();
        Self::join(
            cx,
            q,
            lane,
            worker::Session::Controller(session),
            network.transport(),
            Clock::Controller(network),
            Clock::Controller(worker),
        )
    }
    /// Called by a containing session's existing QUIC driver on every I/O poll.
    /// Between turns call `service` first so an optional stop can retire just
    /// this lane. A loss DURING I/O retains connection-wide fail-closed behavior.
    pub(crate) fn permits_io(&self) -> bool {
        self.retired || self.admission().is_ok()
    }
    pub(crate) fn owns_inbound(&self, route: quic::Route) -> bool {
        self.lane.owns_inbound(route)
    }
    /// Immediately fence queued work and the native final publication check.
    /// This neither waits for the worker nor claims its cleanup has completed.
    pub fn stop(&self) {
        self.shared.stop();
    }
    fn join(
        cx: Cx,
        q: &QuicRecords,
        lane: ClipboardChannel,
        session: worker::Session,
        gate: Transport,
        clock: Clock,
        worker_clock: Clock,
    ) -> Result<(Self, WorkerSeed), Error> {
        clock.sample(&cx)?;
        let (a, b) = gate.switches();
        let switches = (a.state(), b.state());
        let shared = Arc::new(Shared {
            maximum: gate.limits().max_control_message_bytes() as usize,
            gate,
            inbox: std::sync::Mutex::default(),
            outbox: std::sync::Mutex::default(),
            inbound_until: std::sync::atomic::AtomicU64::default(),
            wake: std::sync::OnceLock::default(),
        });
        let seed = WorkerSeed {
            session: Some(session),
            shared: shared.clone(),
            clock: worker_clock,
            cx: cx.clone(),
        };
        Ok((
            Self {
                lane,
                connection: q.binding(),
                shared,
                clock,
                cx,
                switches,
                pending: None,
                inflight: std::array::from_fn(|_| None),
                release_outbox: false,
                retired: false,
                reason: None,
            },
            seed,
        ))
    }
    pub fn switches(&self) -> (ClipboardSwitch, ClipboardSwitch) {
        self.shared.gate.switches()
    }
    pub const fn reason(&self) -> Option<Error> {
        self.reason
    }
    pub const fn is_retired(&self) -> bool {
        self.retired
    }
    /// A terminal publication/refusal is never overwritten by another incoming
    /// item. Collect it even after shutdown; it is not permission to retry.
    pub fn take_received(&mut self) -> Result<Option<Received>, Error> {
        let mut inbox = match self.shared.inbox.try_lock() {
            Ok(v) => v,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(Error::Poisoned),
        };
        let result = inbox.result.take();
        if result.is_some() {
            inbox.busy = false;
            self.shared.wake();
        }
        Ok(result)
    }
    fn bound(&self, q: &QuicRecords) -> Result<(), Error> {
        if !q.is_bound_to(&self.connection) {
            return Err(Error::WrongConnection);
        }
        Ok(())
    }
    /// Fence native publication FIRST, then retire only the optional lane on its
    /// original connection. Never wait for a potentially hung native destructor.
    pub fn retire(&mut self, q: &mut QuicRecords) -> Result<(), Error> {
        self.bound(q)?;
        self.shared.stop();
        self.pending = None;
        self.inflight.fill(None);
        self.release_outbox = false;
        if !self.retired {
            self.retired = true;
            self.lane.retire(q, &self.cx).map_err(Error::Transport)?;
        }
        Ok(())
    }
    fn admission(&self) -> Result<(u64, HostInstant), Error> {
        let (local, host) = self.clock.sample(&self.cx)?;
        if !self.shared.gate.is_open() {
            return Err(Error::Closed);
        }
        let (a, b) = self.shared.gate.switches();
        if (a.state(), b.state()) != self.switches || !a.is_enabled() || !b.is_enabled() {
            return Err(Error::SwitchChanged);
        }
        for (permit, until) in self.inflight.iter().flatten() {
            permit.check_operation(host).map_err(Error::Session)?;
            if local >= *until {
                return Err(Error::HandoffExpired);
            }
        }
        if let Some(p) = &self.pending {
            p.permit.check_operation(host).map_err(Error::Session)?;
            if local >= p.until {
                return Err(Error::HandoffExpired);
            }
        }
        let until = self
            .shared
            .inbound_until
            .load(std::sync::atomic::Ordering::Acquire);
        if until != 0 && local >= until {
            return Err(Error::HandoffExpired);
        }
        Ok((local, host))
    }
    fn release_handoff(&mut self) -> Result<(), Error> {
        if self.release_outbox {
            match self.shared.outbox.try_lock() {
                Ok(mut outbox) => {
                    outbox.busy = false;
                    self.release_outbox = false;
                    self.shared.wake();
                }
                Err(std::sync::TryLockError::WouldBlock) => {}
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(Error::Poisoned),
            }
        }
        Ok(())
    }
    /// Bounded network turn. Native work happens only in `Worker::step`/`WorkerSeed::spawn`;
    /// dispatch callbacks copy one bounded record with a fixed ingress deadline.
    pub fn service(
        &mut self,
        q: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<State, Error> {
        self.bound(q)?;
        if self.retired {
            return Ok(State::Retired);
        }
        let mut turn = Turn::new(&self.shared);
        // Retire before q.tick evaluates an expired retained clipboard record,
        // preserving input/media when the stop happens BETWEEN I/O operations.
        if let Err(reason) = self.admission() {
            self.reason = Some(reason);
            self.retire(q)?;
            turn.complete();
            return Ok(State::Retired);
        }
        q.tick(&self.cx, &mut authorize).map_err(Error::Transport)?;
        if self.lane.check(q).is_err() {
            self.reason = Some(Error::Closed);
            self.retire(q)?;
            turn.complete();
            return Ok(State::Retired);
        }
        if self.inflight.iter().any(Option::is_some)
            && self.lane.send_drained(q).map_err(Error::Transport)?
        {
            self.inflight.fill(None);
        }
        self.release_handoff()?;
        if self.pending.is_none()
            && !self.release_outbox
            && self.inflight.iter().any(Option::is_none)
        {
            match self.shared.outbox.try_lock() {
                Ok(mut outbox) => self.pending = outbox.item.take(),
                Err(std::sync::TryLockError::WouldBlock) => {}
                Err(std::sync::TryLockError::Poisoned(_)) => return Err(Error::Poisoned),
            }
        }
        let (local, host) = match self.admission() {
            Ok(v) => v,
            Err(reason) => {
                self.reason = Some(reason);
                self.retire(q)?;
                turn.complete();
                return Ok(State::Retired);
            }
        };
        if let Some(p) = &self.pending {
            match self.lane.send(q, &self.cx, &p.bytes.0, p.until, || {
                self.admission().is_ok() && authorize()
            }) {
                Ok(()) => {
                    let p = self.pending.take().expect("accepted exact record");
                    let slot = self
                        .inflight
                        .iter_mut()
                        .find(|slot| slot.is_none())
                        .ok_or(Error::Limit)?;
                    *slot = Some((p.permit, p.until));
                    // Admission releases the one worker slot, not the original
                    // Egress permit. If its lock is contended, remember this
                    // exact accepted handoff; never send the record again.
                    self.release_outbox = true;
                    self.release_handoff()?;
                }
                Err(quic::Error::Backpressure) => {}
                Err(e) => return Err(Error::Transport(e)),
            }
        }
        self.lane
            .dispatch(q, &self.cx, &mut authorize, |bytes| {
                self.shared.accept(bytes, local, host)
            })
            .map_err(Error::Transport)?;
        turn.complete();
        Ok(State::Active)
    }
    /// Drives the EXISTING connection. Keep the containing session's admission
    /// callback and service its other routes separately. Cancellation or authority
    /// loss DURING an active native QUIC future remains connection-wide fail-closed.
    /// Even dropping this future unpolled fences the original worker/connection.
    pub fn drive<'a>(
        &'a mut self,
        q: &'a mut QuicRecords,
        wait: Duration,
        mut authorize: impl FnMut() -> bool + 'a,
    ) -> impl std::future::Future<Output = Result<State, Error>> + 'a {
        let bound = self.bound(q);
        let mut guard = bound.is_ok().then(|| Drive {
            q,
            shared: self.shared.clone(),
            complete: false,
        });
        async move {
            bound?;
            let io = guard.as_mut().expect("bound connection");
            self.service(io.q, &mut authorize)?;
            io.q.drive(&self.cx, wait, || {
                (self.retired || self.admission().is_ok()) && authorize()
            })
            .await
            .map_err(Error::Transport)?;
            let state = self.service(io.q, &mut authorize)?;
            io.complete = true;
            Ok(state)
        }
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.shared.stop();
    }
}
struct Drive<'a> {
    q: &'a mut QuicRecords,
    shared: Arc<Shared>,
    complete: bool,
}
impl Drop for Drive<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.shared.stop();
            self.q.close();
        }
    }
}
