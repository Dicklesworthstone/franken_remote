//! One bounded native input owner, paired with an independently polled watchdog.
//!
//! Create one `Seat` per OS share session. Keep polling the returned `Driver` in
//! its Asupersync authority region; the factory creates a thread-confined native
//! sink on a separate OS thread. This is a foreign-call executor, not another
//! async runtime. No actor mailbox lock is held during native or policy work.
//!
//! There is at most ONE queued, executing, or uncollected command per agent.
//! Local lifecycle stop never queues behind it. Timed-out native calls cannot
//! be rolled back or safely killed as threads: they retain the seat until both
//! cleanup layers and native destruction finish. The platform process supervisor
//! must handle process death; this module never claims release after a crash.
mod result;
use result::ResultContext;
pub use result::{InputReply, InputResponse};

use crate::input_watchdog::{self, Control, StopReason, Watchdog};
use asupersync::{
    cx::Cx,
    time::{TimerDriverHandle, TimerHandle},
    types::Time,
};
use fr_core::{
    ids::InputTicketId,
    input_submission::{
        Cleanup, Dispatch, InputSession, InputSink, PlatformError, Receipt, Reconciliation, Refusal,
    },
    limits::ProtocolLimits,
    time::HostInstant,
};
use fr_wire::{
    WireError,
    input::{InputDelivery, InputDirection, MAX_INPUT_RECORD_BYTES, decode_input},
};
use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    thread::{self, Thread},
    time::Duration,
};

const NATIVE_POLL: Duration = Duration::from_millis(10);
const DRAIN_POLL_NS: u64 = 10_000_000;

/// The containing OS share-session owner must share this same seat with ALL
/// contenders. A fresh seat is not a way to bypass uncertain prior cleanup.
#[derive(Clone, Default)]
pub struct Seat(Arc<AtomicBool>);
impl Seat {
    pub fn is_occupied(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Takes ownership of an already locally admitted session. `factory` must
    /// not inject input during initialization and must construct the exact
    /// locally probed display/capabilities used to grant `session`. It executes
    /// on the native thread, so the resulting sink need not be Send.
    /// `native_cleanup` is release/restoration-only and must return true only
    /// when the backend's additional native obligations are resolved.
    pub fn start<S, F, C>(
        &self,
        cx: Cx,
        session: InputSession,
        route: Route,
        factory: F,
        native_cleanup: C,
    ) -> Result<(Agent, Driver), Error>
    where
        S: InputSink + 'static,
        F: FnOnce() -> Result<S, PlatformError> + Send + 'static,
        C: FnMut(&mut S) -> bool + Send + 'static,
    {
        self.start_inner(
            cx,
            session,
            route,
            factory,
            native_cleanup,
            AdmissionGate::default(),
        )
    }
    /// Start only with a currently control-capable installed-Tailscale admission.
    /// This does not create local consent, a control lease, a view or a ticket.
    /// The containing session keeps `Admission` alive and refreshes it independently.
    #[cfg(target_os = "linux")]
    pub fn start_admitted<S, F, C>(
        &self,
        cx: Cx,
        session: InputSession,
        route: Route,
        admission: fr_tailnet::Lease,
        factory: F,
        native_cleanup: C,
    ) -> Result<(Agent, Driver), Error>
    where
        S: InputSink + 'static,
        F: FnOnce() -> Result<S, PlatformError> + Send + 'static,
        C: FnMut(&mut S) -> bool + Send + 'static,
    {
        admission.control().map_err(Error::Admission)?;
        self.start_inner(
            cx,
            session,
            route,
            factory,
            native_cleanup,
            AdmissionGate {
                tailnet: Some(admission),
            },
        )
    }
    fn start_inner<S, F, C>(
        &self,
        cx: Cx,
        session: InputSession,
        route: Route,
        factory: F,
        native_cleanup: C,
        admission: AdmissionGate,
    ) -> Result<(Agent, Driver), Error>
    where
        S: InputSink + 'static,
        F: FnOnce() -> Result<S, PlatformError> + Send + 'static,
        C: FnMut(&mut S) -> bool + Send + 'static,
    {
        if self
            .0
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::SeatBusy);
        }
        let watchdog = match Watchdog::new(cx.clone(), session.monitor()) {
            Ok(w) => w,
            Err(e) => {
                self.0.store(false, Ordering::Release);
                return Err(Error::Clock(e));
            }
        };
        let control = watchdog.control();
        let shared = Arc::new(Shared {
            mailbox: Mutex::new(Mailbox::default()),
            control: control.clone(),
            admission,
        });
        let driver = cx
            .timer_driver()
            .expect("watchdog checked the same Cx timer");
        let runner = shared.clone();
        let seat = self.clone();
        let clock = driver.clone();
        let worker = thread::Builder::new()
            .name("fr-input-native".into())
            .spawn(move || {
                // Includes native sink/factory/cleanup destruction. Handoff never
                // precedes destructors, including a blocking or panicking Drop.
                let exit = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    native_loop(&cx, session, route, factory, native_cleanup, &runner)
                }))
                .unwrap_or(Exit {
                    cleanup: None,
                    native_clean: false,
                    panicked: true,
                });
                runner.control.stop(StopReason::NativeFailure);
                // No native resources or user closures survive native_loop here.
                let wakes = {
                    let mut m = runner.lock();
                    if m.outstanding && m.reply.is_none() {
                        m.reply = Some(Reply::NativePanic { receipt: None });
                    }
                    m.exit = Some(exit);
                    m.phase = Phase::Finished;
                    // Release exactly once, while publishing the terminal state.
                    // A previous owner must never clear a successor's reservation.
                    if exit.handoff_safe() {
                        seat.0.store(false, Ordering::Release);
                    }
                    (m.reply_waker.take(), m.driver_waker.take())
                };
                wake(wakes.0);
                wake(wakes.1);
            });
        let Ok(worker) = worker else {
            self.0.store(false, Ordering::Release);
            return Err(Error::ThreadSpawn);
        };
        let native = worker.thread().clone();
        // The native finalizer owns the reservation even after either public
        // handle is dropped. Thread detach is NOT a successful shutdown claim.
        drop(worker);
        let agent = Agent {
            shared: shared.clone(),
            native: native.clone(),
            route,
            response_context: None,
        };
        let driver = Driver {
            watchdog,
            shared,
            native,
            clock,
            timer: None,
            stopping: None,
            last: None,
            done: None,
        };
        Ok((agent, driver))
    }
}
/// Locally established transport binding and downward-negotiated bounds. The
/// sender cannot select role or a different binding in the record itself.
#[derive(Clone, Copy)]
pub struct Route {
    binding: u32,
    limits: ProtocolLimits,
}
impl Route {
    pub const fn new(binding: u32, limits: ProtocolLimits) -> Self {
        Self { binding, limits }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    SeatBusy,
    ThreadSpawn,
    Stopped,
    Backpressure,
    NoPendingCommand,
    NotInputCommand,
    RecordTooLarge,
    Wire(WireError),
    Clock(input_watchdog::Error),
    #[cfg(target_os = "linux")]
    Admission(fr_tailnet::Error),
}
/// These commands are issued by the local authenticated session owner, not
/// arbitrary remote requests. Receipt of traffic never implicitly renews.
pub enum AuthorityCommand {
    ObservationChallenge(u128),
    ObservationResponse(u128),
    ControlChallenge(u128),
    ControlResponse(u128),
    Ticket(InputTicketId),
}
/// No input payload, credential, native error string, or nonce is retained in a
/// reply. Reliable panic receipts retain the exact confirmed operation prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reply {
    Input(Result<Dispatch, Refusal>),
    Authority(Result<HostInstant, Refusal>),
    Reconciliation(Result<Reconciliation, Refusal>),
    ReconciliationPanic { report: Option<Reconciliation> },
    CancelledBeforeStart,
    NativePanic { receipt: Option<Receipt> },
    InitializationFailed(PlatformError),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    pub cleanup: Option<Cleanup>,
    pub native_clean: bool,
    pub panicked: bool,
}
impl Exit {
    pub fn handoff_safe(self) -> bool {
        !self.panicked && self.native_clean && self.cleanup.is_some_and(|c| c.remaining == 0)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub phase: Phase,
    pub outstanding: bool,
    pub stopped: bool,
    pub cleanup: Option<Cleanup>,
    pub native_clean: bool,
    pub exit: Option<Exit>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shutdown {
    pub reason: StopReason,
    /// None means the worker has not finished, NOT that there is no held input.
    pub exit: Option<Exit>,
    pub last_cleanup: Option<Cleanup>,
}
impl Shutdown {
    pub fn handoff_safe(self) -> bool {
        self.exit.is_some_and(Exit::handoff_safe)
    }
}

enum CommandKind {
    Input(InputDelivery),
    Reconcile,
    Authority(AuthorityCommand),
}
// Fixed inline record storage bounds payload AND metadata; no per-record heap
// allocation and no vector with excess hidden capacity is sent to the native sink.
struct Command {
    kind: CommandKind,
    bytes: [u8; MAX_INPUT_RECORD_BYTES],
    length: usize,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Starting,
    Running,
    Cleaning,
    Finished,
}
struct CleanupAttempt {
    core: Cleanup,
    native_clean: bool,
}
#[derive(Default)]
struct Mailbox {
    command: Option<Command>,
    reply: Option<Reply>,
    outstanding: bool,
    phase: Phase,
    abandoned: bool,
    retry_cleanup: bool,
    cleanup: Option<CleanupAttempt>,
    exit: Option<Exit>,
    reply_waker: Option<Waker>,
    driver_waker: Option<Waker>,
}
#[derive(Default)]
struct AdmissionGate {
    #[cfg(target_os = "linux")]
    tailnet: Option<fr_tailnet::Lease>,
}
impl AdmissionGate {
    fn permitted(&self) -> bool {
        #[cfg(target_os = "linux")]
        if let Some(lease) = &self.tailnet {
            return lease.control().is_ok();
        }
        true
    }
}
struct Shared {
    admission: AdmissionGate,
    mailbox: Mutex<Mailbox>,
    control: Control,
}
impl Shared {
    fn check_admission(&self) {
        // Only a finite in-memory gate is checked here; no LocalAPI I/O or
        // mailbox lock. Revocation precedes waking/draining the native owner.
        if !self.admission.permitted() {
            self.control.stop(StopReason::AuthorityEnded);
        }
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Mailbox> {
        // Only bounded assignments occur under this lock, never caller code,
        // native code, policy callbacks, clock reads, waker callbacks or awaits.
        self.mailbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn reply(&self, reply: Reply) {
        let w = {
            let mut m = self.lock();
            m.reply = Some(reply);
            m.reply_waker.take()
        };
        wake(w);
    }
    fn abandon(&self) {
        self.control.stop(StopReason::ClientDisconnected);
        let mut m = self.lock();
        m.abandoned = true;
    }
}
fn wake(waker: Option<Waker>) {
    if let Some(w) = waker {
        w.wake();
    }
}

pub struct Agent {
    shared: Arc<Shared>,
    native: Thread,
    route: Route,
    response_context: Option<ResultContext>,
}
impl Agent {
    /// Locally installed channel, never read from an input payload.
    pub const fn channel_binding(&self) -> u32 {
        self.route.binding
    }
    pub const fn protocol_limits(&self) -> ProtocolLimits {
        self.route.limits
    }
    pub fn control(&self) -> Control {
        self.shared.control.clone()
    }
    pub fn status(&self) -> Status {
        let m = self.shared.lock();
        Status {
            phase: m.phase,
            outstanding: m.outstanding,
            stopped: self.shared.control.is_stopped(),
            cleanup: m.cleanup.as_ref().map(|c| c.core),
            native_clean: m.cleanup.as_ref().is_some_and(|c| c.native_clean),
            exit: m.exit,
        }
    }
    /// At most one record exists until its reply is collected, even if native
    /// submission already finished. Validate before copying any untrusted bytes.
    pub fn submit(&mut self, bytes: &[u8], delivery: InputDelivery) -> Result<(), Error> {
        self.shared.check_admission();
        if self.shared.control.is_stopped() {
            return Err(Error::Stopped);
        }
        if bytes.len() > MAX_INPUT_RECORD_BYTES {
            return Err(Error::RecordTooLarge);
        }
        let request = decode_input(
            bytes,
            &self.route.limits,
            self.route.binding,
            InputDirection::ViewerToHost,
            delivery,
        )
        .map_err(Error::Wire)?;
        let mut command = Command {
            kind: CommandKind::Input(delivery),
            bytes: [0; MAX_INPUT_RECORD_BYTES],
            length: bytes.len(),
        };
        command.bytes[..bytes.len()].copy_from_slice(bytes);
        let context = ResultContext::new(self.route.binding, request);
        self.enqueue(command)?;
        // Mutate only after successful admission. A refused second command
        // must never replace the original uncollected result's binding.
        self.response_context = Some(context);
        Ok(())
    }
    /// Queue a release-only snapshot in the SAME bounded slot as native input.
    /// It cannot create a press, renew control, or masquerade as an `InputResult`.
    pub fn reconcile_held(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.shared.check_admission();
        if self.shared.control.is_stopped() {
            return Err(Error::Stopped);
        }
        if bytes.len() > MAX_INPUT_RECORD_BYTES {
            return Err(Error::RecordTooLarge);
        }
        fr_wire::held_state::decode(
            bytes,
            &self.route.limits,
            self.route.binding,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        let mut command = Command {
            kind: CommandKind::Reconcile,
            bytes: [0; MAX_INPUT_RECORD_BYTES],
            length: bytes.len(),
        };
        command.bytes[..bytes.len()].copy_from_slice(bytes);
        self.enqueue(command)?;
        self.response_context = None;
        Ok(())
    }
    pub fn authority(&mut self, command: AuthorityCommand) -> Result<(), Error> {
        self.enqueue(Command {
            kind: CommandKind::Authority(command),
            bytes: [0; MAX_INPUT_RECORD_BYTES],
            length: 0,
        })?;
        self.response_context = None;
        Ok(())
    }
    fn enqueue(&mut self, command: Command) -> Result<(), Error> {
        self.shared.check_admission();
        {
            let mut m = self.shared.lock();
            if self.shared.control.is_stopped() || m.exit.is_some() {
                return Err(Error::Stopped);
            }
            if m.outstanding {
                return Err(Error::Backpressure);
            }
            m.outstanding = true;
            m.command = Some(command);
        }
        self.native.unpark();
        Ok(())
    }
    pub fn try_reply(&mut self) -> Result<Option<Reply>, Error> {
        let mut m = self.shared.lock();
        if !m.outstanding {
            return Err(Error::NoPendingCommand);
        }
        let reply = m.reply.take();
        if reply.is_some() {
            m.outstanding = false;
            m.reply_waker = None;
            self.response_context = None;
        }
        Ok(reply)
    }
    /// Cancelling this wait revokes input but never discards the actual reply.
    /// Collect it later with `try_reply`/`response`; never transparently retry input.
    pub fn response(&mut self) -> Response<'_> {
        Response {
            agent: self,
            done: false,
        }
    }
    /// Local-only request; coalesced to one flag, not an unbounded retry queue.
    pub fn retry_cleanup(&self) -> Result<(), Error> {
        if !self.shared.control.is_stopped() {
            return Err(Error::Stopped);
        }
        {
            let mut m = self.shared.lock();
            if m.exit.is_some() {
                return Err(Error::Stopped);
            }
            m.retry_cleanup = true;
        }
        self.native.unpark();
        Ok(())
    }
}
impl Drop for Agent {
    fn drop(&mut self) {
        self.shared.abandon();
        self.native.unpark();
    }
}
#[must_use = "await the reply; abandoning the wait revokes input"]
pub struct Response<'a> {
    agent: &'a mut Agent,
    done: bool,
}
impl Future for Response<'_> {
    type Output = Result<Reply, Error>;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let outcome = {
            let mut m = this.agent.shared.lock();
            if !m.outstanding {
                Some(Err(Error::NoPendingCommand))
            } else if let Some(reply) = m.reply.take() {
                m.outstanding = false;
                m.reply_waker = None;
                Some(Ok(reply))
            } else {
                m.reply_waker = Some(task.waker().clone());
                None
            }
        };
        if let Some(result) = outcome {
            this.agent.response_context = None;
            this.done = true;
            Poll::Ready(result)
        } else {
            Poll::Pending
        }
    }
}
impl Drop for Response<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.agent.shared.control.stop(StopReason::Cancelled);
            self.agent.shared.lock().reply_waker = None;
            self.agent.native.unpark();
        }
    }
}

/// Poll independently in the authority region. Stops never wait for native I/O.
/// On stop, drain for a bounded interval; an unresolved worker retains its seat
/// even when this future returns an uncertain shutdown result.
#[must_use = "poll in the authority region independently of native submission"]
pub struct Driver {
    watchdog: Watchdog,
    shared: Arc<Shared>,
    native: Thread,
    clock: TimerDriverHandle,
    timer: Option<(TimerHandle, Time, Waker)>,
    stopping: Option<Time>,
    last: Option<Time>,
    done: Option<Shutdown>,
}
impl Driver {
    pub fn control(&self) -> Control {
        self.shared.control.clone()
    }
    fn finish(&mut self, reason: StopReason) -> Poll<Shutdown> {
        if let Some((handle, _, _)) = self.timer.take() {
            let _ = self.clock.cancel(&handle);
        }
        let report = {
            let mut m = self.shared.lock();
            m.driver_waker = None;
            Shutdown {
                reason,
                exit: m.exit,
                last_cleanup: m.cleanup.as_ref().map(|c| c.core),
            }
        };
        self.done = Some(report);
        Poll::Ready(report)
    }
}
impl Future for Driver {
    type Output = Shutdown;
    fn poll(self: Pin<&mut Self>, task: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(done) = this.done {
            return Poll::Ready(done);
        }
        this.shared.check_admission();
        this.shared.lock().driver_waker = Some(task.waker().clone());
        let Poll::Ready(reason) = Pin::new(&mut this.watchdog).poll(task) else {
            return Poll::Pending;
        };
        this.native.unpark();
        let now = this.clock.now();
        if this.shared.lock().exit.is_some() {
            return this.finish(reason);
        }
        if this.last.is_some_and(|last| now < last) {
            return this.finish(StopReason::ClockRegression);
        }
        this.last = Some(now);
        // One second from the FIRST observed stop, never reset by traffic,
        // partial native progress or repeated polls.
        let until = if let Some(until) = this.stopping {
            until
        } else {
            let Some(n) = now.as_nanos().checked_add(1_000_000_000) else {
                return this.finish(StopReason::ClockOverflow);
            };
            let until = Time::from_nanos(n);
            this.stopping = Some(until);
            until
        };
        if now >= until {
            return this.finish(reason);
        }
        let at = Time::from_nanos(now.as_nanos().saturating_add(DRAIN_POLL_NS)).min(until);
        if !this
            .timer
            .as_ref()
            .is_some_and(|(_, old, w)| now < *old && *old <= at && w.will_wake(task.waker()))
        {
            if let Some((handle, _, _)) = this.timer.take() {
                let _ = this.clock.cancel(&handle);
            }
            this.timer = Some((
                this.clock.register(at, task.waker().clone()),
                at,
                task.waker().clone(),
            ));
        }
        Poll::Pending
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        if let Some((timer, _, _)) = self.timer.take() {
            let _ = self.clock.cancel(&timer);
        }
        if self.done.is_none() {
            self.shared.abandon();
        }
        self.shared.lock().driver_waker = None;
        self.native.unpark();
    }
}

fn native_loop<S: InputSink, F: FnOnce() -> Result<S, PlatformError>, C: FnMut(&mut S) -> bool>(
    cx: &Cx,
    mut session: InputSession,
    route: Route,
    factory: F,
    mut native_cleanup: C,
    shared: &Shared,
) -> Exit {
    let mut sink = match factory() {
        Ok(s) => s,
        Err(error) => {
            shared.control.stop(StopReason::NativeFailure);
            if shared.lock().outstanding {
                shared.reply(Reply::InitializationFailed(error));
            }
            return Exit {
                cleanup: Some(Cleanup {
                    submitted_releases: 0,
                    remaining: 0,
                }),
                native_clean: true,
                panicked: false,
            };
        }
    };
    shared.lock().phase = Phase::Running;
    let mut last = None;
    loop {
        shared.check_admission();
        let now = input_watchdog::host_now(cx).expect("captured Cx retains its timer");
        if cx.checkpoint().is_err() {
            shared.control.stop(StopReason::Cancelled);
        }
        if last.is_some_and(|t| now < t) {
            shared.control.stop(StopReason::ClockRegression);
        }
        last = Some(now);
        if session.monitor().deadline(now).is_err() {
            shared.control.stop(StopReason::AuthorityEnded);
        }
        let command = shared.lock().command.take();
        if shared.control.is_stopped() {
            if command.is_some() {
                shared.reply(Reply::CancelledBeforeStart);
            }
            break;
        }
        if let Some(command) = command {
            execute(&mut session, &mut sink, cx, command, route, shared);
        } else {
            thread::park_timeout(NATIVE_POLL);
        }
    }
    session.revoke();
    shared.lock().phase = Phase::Cleaning;
    loop {
        // Releases do not require a remote ticket. Both layers are checked on
        // every explicit local retry, and incomplete native state remains owned.
        let cleanup = session.cleanup(&mut sink);
        sink.cancel_prepared();
        let native_clean = native_cleanup(&mut sink);
        let (abandoned, waker) = {
            let mut m = shared.lock();
            m.cleanup = Some(CleanupAttempt {
                core: cleanup,
                native_clean,
            });
            (m.abandoned, m.driver_waker.take())
        };
        wake(waker);
        if (cleanup.remaining == 0 && native_clean) || abandoned {
            // Explicitly destroy native resources BEFORE the outer finalizer can
            // free the seat or publish terminal completion.
            drop(sink);
            drop(native_cleanup);
            drop(session);
            return Exit {
                cleanup: Some(cleanup),
                native_clean,
                panicked: false,
            };
        }
        loop {
            let retry = {
                let mut m = shared.lock();
                let retry = m.retry_cleanup || m.abandoned;
                m.retry_cleanup = false;
                retry
            };
            if retry {
                break;
            }
            thread::park_timeout(NATIVE_POLL);
        }
    }
}
fn execute<S: InputSink>(
    session: &mut InputSession,
    sink: &mut S,
    cx: &Cx,
    command: Command,
    route: Route,
    shared: &Shared,
) {
    let mut reliable_sequence = None;
    let reconciling = matches!(command.kind, CommandKind::Reconcile);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match command.kind {
        CommandKind::Input(delivery) => {
            let request = decode_input(
                &command.bytes[..command.length],
                &route.limits,
                route.binding,
                InputDirection::ViewerToHost,
                delivery,
            )
            .expect("immutable record was validated before admission");
            if !request.event.is_pointer() {
                reliable_sequence = Some(request.sequence);
            }
            Reply::Input(session.dispatch(request, sink, || {
                shared.check_admission();
                // This callback runs AFTER every native preparation. The
                // independent watchdog may not yet have been scheduled after
                // parent cancellation; never let that lag authorize a press
                // or the next scalar of a partially submitted text operation.
                if cx.checkpoint().is_err() {
                    shared.control.stop(StopReason::Cancelled);
                }
                input_watchdog::host_now(cx).expect("captured timer")
            }))
        }
        CommandKind::Reconcile => {
            let request = fr_wire::held_state::decode(
                &command.bytes[..command.length],
                &route.limits,
                route.binding,
                InputDirection::ViewerToHost,
                InputDelivery::Reliable,
            )
            .expect("immutable held-state record was validated before admission");
            Reply::Reconciliation(session.reconcile_held(request, sink, || {
                shared.check_admission();
                if cx.checkpoint().is_err() {
                    shared.control.stop(StopReason::Cancelled);
                }
                input_watchdog::host_now(cx).expect("captured timer")
            }))
        }
        CommandKind::Authority(command) => {
            shared.check_admission();
            if cx.checkpoint().is_err() {
                shared.control.stop(StopReason::Cancelled);
            }
            let now = input_watchdog::host_now(cx).expect("captured timer");
            Reply::Authority(match command {
                AuthorityCommand::ObservationChallenge(n) => {
                    session.issue_observation_challenge(n, now)
                }
                AuthorityCommand::ObservationResponse(n) => session.renew_observation(n, now),
                AuthorityCommand::ControlChallenge(n) => session.issue_control_challenge(n, now),
                AuthorityCommand::ControlResponse(n) => session.renew_control(n, now),
                AuthorityCommand::Ticket(t) => session.issue_ticket(t, now),
            })
        }
    }));
    let reply = result.unwrap_or_else(|_| {
        shared.control.stop(StopReason::NativeFailure);
        if reconciling {
            Reply::ReconciliationPanic {
                report: session.retained_reconciliation(),
            }
        } else {
            Reply::NativePanic {
                receipt: reliable_sequence.and_then(|s| session.retained_receipt(s)),
            }
        }
    });
    shared.reply(reply);
}
