//! One admitted QUIC input attachment joined to the canonical native agent.
//! No listener, identity, grant, timer domain, or replacement input executor is
//! created here. Poll the agent's Driver independently in its authority region.
//! Session admission supplies routes and the connection-level authorization
//! callback. A native result may be sent after input revocation: it describes an
//! already committed effect, not permission to perform another operation.
use crate::{
    input_agent::{self, Agent, AuthorityCommand, InputReply, Reply, Status},
    input_watchdog::{Control, StopReason},
};
use asupersync::cx::Cx;
use fr_core::{input_submission::Refusal, limits::ProtocolLimits, time::HostInstant};
use fr_transport::quic::{
    self, ConnectionBinding, DatagramRoute, Disposition, Messages, Priority, QuicRecords, Route,
    StreamRoute,
};
use fr_wire::{
    Kind, WireError,
    input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES},
    input_result::{INPUT_RESULT_BYTES, InputResult, encode_input_result},
    input_ticket::{INPUT_TICKET_BYTES, Ticket},
};
use std::{cell::Cell, time::Duration};

pub mod control;
pub mod grant;
mod negotiated;
pub use negotiated::NegotiatedInput;
mod ticket;

const RECEIPT_LIFETIME_US: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidRoutes,
    AgentBusy,
    WrongConnection,
    Closed,
    Clock,
    InvalidTicketId,
    TicketRefused(Refusal),
    TicketExpired,
    Agent(input_agent::Error),
    Transport(quic::Error),
    Wire(WireError),
    /// The actual agent could not provide a terminal receipt. Never replace
    /// this with a fabricated zero-effect result or replay the input.
    ReceiptUnavailable(InputReply),
}

/// Host-side routes installed by the authenticated session. All reliable input
/// kinds share one client-initiated stream; results use the reverse direction.
#[derive(Debug, Clone, Copy)]
pub struct Routes {
    actions: StreamRoute,
    results: StreamRoute,
    pointer: Option<DatagramRoute>,
}
impl Routes {
    pub fn new(
        actions: StreamRoute,
        results: StreamRoute,
        pointer: Option<DatagramRoute>,
    ) -> Result<Self, Error> {
        if actions.outbound
            || !results.outbound
            || actions.binding == 0
            || actions.binding != results.binding
            || actions.stream == results.stream
            || actions.messages != Messages::InputActions
            || !matches!(
                results.messages,
                Messages::Exact(0x0048) | Messages::InputFeedback
            )
            || (results.messages == Messages::InputFeedback && results.maximum < INPUT_TICKET_BYTES)
            || actions.priority != Priority::Critical
            || results.priority != Priority::Critical
            || actions.maximum > MAX_INPUT_RECORD_BYTES
            || results.maximum < INPUT_RESULT_BYTES
            || pointer
                .is_some_and(|p| p.outbound || p.binding != actions.binding || p.kind != 0x0042)
        {
            return Err(Error::InvalidRoutes);
        }
        Ok(Self {
            actions,
            results,
            pointer,
        })
    }
    pub const fn actions(self) -> StreamRoute {
        self.actions
    }
    pub const fn results(self) -> StreamRoute {
        self.results
    }
    pub const fn pointer(self) -> Option<DatagramRoute> {
        self.pointer
    }
    fn input(self, route: Route) -> Option<InputDelivery> {
        if route == Route::Stream(self.actions) {
            Some(InputDelivery::Reliable)
        } else if self.pointer.is_some_and(|p| route == Route::Datagram(p)) {
            Some(InputDelivery::Datagram)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    Idle,
    NativePending,
    ReceiptBackpressure,
    TicketBackpressure,
    /// Transport admission only; the viewer must still validate expiry/view.
    TicketQueued(Ticket),
    /// QUIC accepted these receipt bytes. This is not peer delivery or an
    /// upgrade of the receipt's native submission stage to observation.
    ReceiptQueued(InputResult),
    ObsoletePointer,
    Authority(Result<HostInstant, Refusal>),
    /// Local native reconciliation outcome, not a peer acknowledgement.
    Reconciliation(Reply),
    Stopped,
}
#[derive(Clone, Copy)]
enum Command {
    Input,
    Reconcile,
    Authority,
    Ticket,
}
#[derive(Clone, Copy)]
enum Feedback {
    Result(InputResult),
    Ticket(Ticket),
}
struct Pending {
    feedback: Feedback,
    bytes: [u8; INPUT_TICKET_BYTES],
    length: usize,
    until: u64,
}
impl Pending {
    const fn backpressure(&self) -> Progress {
        match self.feedback {
            Feedback::Result(_) => Progress::ReceiptBackpressure,
            Feedback::Ticket(_) => Progress::TicketBackpressure,
        }
    }
    const fn queued(&self) -> Progress {
        match self.feedback {
            Feedback::Result(result) => Progress::ReceiptQueued(result),
            Feedback::Ticket(ticket) => Progress::TicketQueued(ticket),
        }
    }
}
/// Owns ONE agent, one connection identity, and at most one fixed-size unsent
/// feedback record. While a command or feedback is pending, no further native input is
/// admitted. The bounded QUIC receiver retains/backpressures those records.
/// Late results stay available locally even when the connection is lost.
pub struct QuicInput {
    agent: Agent,
    cx: Cx,
    connection: ConnectionBinding,
    routes: Routes,
    limits: ProtocolLimits,
    command: Option<Command>,
    pending: Option<Pending>,
    last_reply: Option<InputReply>,
    last_reconciliation: Option<Reply>,
    ticket_sequence: Option<u64>,
    ticket_after_us: u64,
    last_ticket: Option<fr_core::ids::InputTicketId>,
}
impl QuicInput {
    /// `cx` must be the same clock/authority region used to create the agent and
    /// the connection. The independently polled native Driver is NOT consumed.
    pub fn new(
        cx: Cx,
        agent: Agent,
        connection: &QuicRecords,
        routes: Routes,
    ) -> Result<Self, Error> {
        if connection.is_closed() {
            return Err(Error::Closed);
        }
        if agent.channel_binding() != routes.actions.binding
            || !connection.has_route(Route::Stream(routes.actions))
            || !connection.has_route(Route::Stream(routes.results))
            || routes
                .pointer
                .is_some_and(|p| !connection.has_route(Route::Datagram(p)))
        {
            return Err(Error::InvalidRoutes);
        }
        if agent.status().outstanding {
            return Err(Error::AgentBusy);
        }
        if cx.timer_driver().is_none() {
            return Err(Error::Clock);
        }
        let limits = agent.protocol_limits();
        Ok(Self {
            agent,
            cx,
            connection: connection.binding(),
            routes,
            limits,
            command: None,
            pending: None,
            last_reply: None,
            last_reconciliation: None,
            ticket_sequence: Some(0),
            ticket_after_us: 0,
            last_ticket: None,
        })
    }
    pub fn control(&self) -> Control {
        self.agent.control()
    }
    pub fn status(&self) -> Status {
        self.agent.status()
    }
    pub const fn last_reply(&self) -> Option<InputReply> {
        self.last_reply
    }
    /// Retains actual release prefixes/unknown effects even after disconnect.
    /// A reconciliation is not an action and has no fabricated `InputResult`.
    pub const fn last_reconciliation(&self) -> Option<Reply> {
        self.last_reconciliation
    }
    pub fn pending_receipt(&self) -> Option<InputResult> {
        self.pending.as_ref().and_then(|p| match p.feedback {
            Feedback::Result(result) => Some(result),
            Feedback::Ticket(_) => None,
        })
    }
    pub fn can_accept_input(&self) -> bool {
        self.command.is_none() && self.pending.is_none() && !self.control().is_stopped()
    }
    /// Local authenticated session operation, never a peer-supplied shortcut
    /// around challenge validation. Its result is returned by `service`.
    pub fn authority(&mut self, command: AuthorityCommand) -> Result<(), Error> {
        if !self.can_accept_input() {
            return Err(Error::Agent(input_agent::Error::Backpressure));
        }
        self.agent.authority(command).map_err(Error::Agent)?;
        self.command = Some(Command::Authority);
        Ok(())
    }
    pub fn retry_cleanup(&self) -> Result<(), Error> {
        self.agent.retry_cleanup().map_err(Error::Agent)
    }
    fn bound(&self, connection: &QuicRecords) -> Result<(), Error> {
        if !connection.is_bound_to(&self.connection) {
            // A stale attachment may revoke its own agent, but must NEVER
            // close a successor's connection or route its old receipts there.
            self.control().stop(StopReason::ClientDisconnected);
            return Err(Error::WrongConnection);
        }
        Ok(())
    }
    fn collect(&mut self) -> Result<Progress, Error> {
        match self.command {
            None => Ok(Progress::Idle),
            Some(Command::Reconcile) => {
                let Some(reply) = self.agent.try_reply().map_err(Error::Agent)? else {
                    return Ok(Progress::NativePending);
                };
                self.command = None;
                self.last_reconciliation = Some(reply);
                Ok(Progress::Reconciliation(reply))
            }
            Some(Command::Authority) => {
                let Some(reply) = self.agent.try_reply().map_err(Error::Agent)? else {
                    return Ok(Progress::NativePending);
                };
                self.command = None;
                let Reply::Authority(result) = reply else {
                    return Err(Error::Agent(input_agent::Error::NotInputCommand));
                };
                Ok(Progress::Authority(result))
            }
            Some(Command::Ticket) => self.collect_ticket(),
            Some(Command::Input) => {
                let Some(reply) = self.agent.try_input_result().map_err(Error::Agent)? else {
                    return Ok(Progress::NativePending);
                };
                self.command = None;
                self.last_reply = Some(reply);
                match reply {
                    InputReply::Record(result) => {
                        let mut bytes = [0; INPUT_TICKET_BYTES];
                        let length = encode_input_result(
                            result,
                            &mut bytes,
                            &self.limits,
                            InputDirection::HostToViewer,
                            InputDelivery::Reliable,
                        )
                        .map_err(Error::Wire)?;
                        let until =
                            self.cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000;
                        let until = until.checked_add(RECEIPT_LIFETIME_US).ok_or(Error::Clock)?;
                        self.pending = Some(Pending {
                            feedback: Feedback::Result(result),
                            bytes,
                            length,
                            until,
                        });
                        Ok(Progress::ReceiptBackpressure)
                    }
                    InputReply::ObsoletePointer => Ok(Progress::ObsoletePointer),
                    other => Err(Error::ReceiptUnavailable(other)),
                }
            }
        }
    }
    /// Collect a completed native reply and admit its exact bytes once. Poll on
    /// idle turns too. Native and transport backpressure are distinct; neither
    /// mints a ticket, replays an action, nor slides a receipt's send deadline.
    /// After connection loss, repeated service still collects the late native
    /// result into `last_reply`/`pending_receipt`, but cannot send it elsewhere.
    pub fn service(
        &mut self,
        connection: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
    ) -> Result<Progress, Error> {
        self.bound(connection)?;
        let mut io = IoGuard::new(connection, self.control());
        let progress = self.collect()?;
        if io.connection.is_closed() {
            return Err(Error::Closed);
        }
        // Check cancellation/connection admission even with no new input bytes.
        io.connection
            .tick(&self.cx, &mut authorize)
            .map_err(Error::Transport)?;
        if io
            .connection
            .receive_ended(self.routes.actions)
            .map_err(Error::Transport)?
        {
            self.control().stop(StopReason::ClientDisconnected);
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|p| matches!(p.feedback, Feedback::Ticket(_)))
        {
            if self.control().is_stopped() {
                self.pending = None;
                return Err(Error::Closed);
            }
            let now = self.cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000;
            if self.pending.as_ref().is_some_and(|p| now >= p.until) {
                self.pending = None;
                return Err(Error::TicketExpired);
            }
        }
        let control = self.control();
        let progress = if let Some(pending) = &self.pending {
            let ticket = matches!(pending.feedback, Feedback::Ticket(_));
            match io.connection.send(
                &self.cx,
                Route::Stream(self.routes.results),
                &pending.bytes[..pending.length],
                pending.until,
                || (!ticket || !control.is_stopped()) && authorize(),
            ) {
                Ok(()) => {
                    let progress = pending.queued();
                    self.pending = None;
                    progress
                }
                Err(quic::Error::Backpressure) => pending.backpressure(),
                Err(e) => return Err(Error::Transport(e)),
            }
        } else if progress == Progress::Idle && self.control().is_stopped() {
            Progress::Stopped
        } else {
            progress
        };
        io.complete = true;
        Ok(progress)
    }
    fn submit(&mut self, route: Route, bytes: &[u8]) -> Result<Disposition, Error> {
        let delivery = self.routes.input(route).ok_or(Error::InvalidRoutes)?;
        if !self.can_accept_input() {
            return Ok(Disposition::Blocked);
        }
        // Dispatch only: the native agent validates the entire immutable record
        // before copying it. A kind byte alone never authorizes reconciliation.
        let reconciliation = delivery == InputDelivery::Reliable
            && bytes
                .get(6..8)
                .is_some_and(|kind| kind == (Kind::HeldState as u16).to_be_bytes());
        let result = if reconciliation {
            self.agent.reconcile_held(bytes)
        } else {
            self.agent.submit(bytes, delivery)
        };
        match result {
            Ok(()) => {
                self.command = Some(if reconciliation {
                    Command::Reconcile
                } else {
                    Command::Input
                });
                Ok(Disposition::Consumed)
            }
            Err(input_agent::Error::Backpressure | input_agent::Error::Stopped) => {
                Ok(Disposition::Blocked)
            }
            Err(e) => Err(Error::Agent(e)),
        }
    }
    /// First give the ordered action stream a bounded receive turn. Then service
    /// all other routes, including pointer states only if no action was admitted.
    /// A pointer flood cannot repeatedly win the one native slot ahead of a
    /// buffered key-up. At most two transport turns (32 records) run here.
    /// Other session/media routes stay with the caller, not this input owner.
    pub fn receive(
        &mut self,
        connection: &mut QuicRecords,
        mut authorize: impl FnMut() -> bool,
        mut other_ready: impl FnMut(Route) -> bool,
        mut other_handler: impl FnMut(Route, &[u8]) -> Result<Disposition, ()>,
    ) -> Result<usize, Error> {
        self.bound(connection)?;
        let mut io = IoGuard::new(connection, self.control());
        let cx = self.cx.clone();
        let routes = self.routes;
        // FIN/RESET is an authority boundary even when the native owner or its
        // receipt is blocked. Inspect metadata without draining a refused lane.
        if io
            .connection
            .receive_ended(routes.actions)
            .map_err(Error::Transport)?
        {
            self.control().stop(StopReason::ClientDisconnected);
        }
        let available = Cell::new(self.can_accept_input());
        let mut fault = None;
        let first = io.connection.receive_ready(
            &cx,
            &mut authorize,
            |r| r == Route::Stream(routes.actions) && available.get(),
            |r, bytes| match self.submit(r, bytes) {
                Ok(d) => {
                    available.set(self.can_accept_input());
                    Ok(d)
                }
                Err(e) => {
                    fault = Some(e);
                    Err(())
                }
            },
        );
        let first = first.map_err(|e| fault.unwrap_or(Error::Transport(e)))?;
        let rest = io.connection.receive_ready(
            &cx,
            &mut authorize,
            |r| {
                if routes.input(r).is_some() {
                    r != Route::Stream(routes.actions) && available.get()
                } else {
                    other_ready(r)
                }
            },
            |r, bytes| {
                if routes.input(r).is_some() {
                    match self.submit(r, bytes) {
                        Ok(d) => {
                            available.set(self.can_accept_input());
                            Ok(d)
                        }
                        Err(e) => {
                            fault = Some(e);
                            Err(())
                        }
                    }
                } else {
                    other_handler(r, bytes)
                }
            },
        );
        let rest = rest.map_err(|e| fault.unwrap_or(Error::Transport(e)))?;
        if io
            .connection
            .receive_ended(routes.actions)
            .map_err(Error::Transport)?
        {
            self.control().stop(StopReason::ClientDisconnected);
        }
        io.complete = true;
        Ok(first + rest)
    }
    /// Drive the actual connection without borrowing native input. Failure or
    /// dropping even an unpolled drive must not leave the native lease alive.
    /// The guard is constructed synchronously, before the async future exists.
    pub fn drive<'a>(
        &'a self,
        connection: &'a mut QuicRecords,
        wait: Duration,
        mut authorize: impl FnMut() -> bool + 'a,
    ) -> impl std::future::Future<Output = Result<(), Error>> + 'a {
        let bound = self.bound(connection);
        let guard = bound
            .is_ok()
            .then(|| IoGuard::new(connection, self.control()));
        async move {
            bound?;
            let mut io = guard.expect("matching connection installs the guard");
            io.connection
                .drive(&self.cx, wait, &mut authorize)
                .await
                .map_err(Error::Transport)?;
            if io
                .connection
                .receive_ended(self.routes.actions)
                .map_err(Error::Transport)?
            {
                self.control().stop(StopReason::ClientDisconnected);
            }
            io.complete = true;
            Ok(())
        }
    }
}
struct IoGuard<'a> {
    connection: &'a mut QuicRecords,
    control: Control,
    complete: bool,
}
impl<'a> IoGuard<'a> {
    fn new(connection: &'a mut QuicRecords, control: Control) -> Self {
        Self {
            connection,
            control,
            complete: false,
        }
    }
}
impl Drop for IoGuard<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.control.stop(StopReason::ClientDisconnected);
            self.connection.close();
        }
    }
}
