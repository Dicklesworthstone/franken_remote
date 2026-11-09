//! In-process presentation for platforms whose native window cannot be shared
//! with a child. One Asupersync drained blocking task owns the presenter for
//! its entire configured lifetime, including destruction. Presenter calls never
//! run in the authority or UI future's poll.
//!
//! The command queue holds one request and its reply is a one-shot. The same
//! work deadline and live authority are checked by the asynchronous owner
//! before publication and on every wait. Cancellation fences replies at once;
//! a foreign call already executing must still return before cleanup can be
//! confirmed. Reaping waits for that exact task, not just a closed channel.
use super::{Deadline, Error, Exchange, State, Worker, bounded, now, runtime_ready};
use asupersync::{
    channel::oneshot,
    cx::Cx,
    process::ExitStatus,
    runtime::{JoinError, spawn_blocking::DrainedBlockingHandle},
    time::sleep_until,
    types::Time,
};
use fr_core::limits::ProtocolLimits;
use fr_media::{
    access_unit::EncodedAccessUnit,
    hevc::{DecoderRecord, HevcGuard},
    worker::{self, Configuration, Identity, Kind, Record, Role, overlay},
};
use std::{
    future::{Future, poll_fn},
    mem::size_of,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    task::Poll,
    time::Duration,
};

/// A native decoder and presenter owned by this process. After launch is
/// admitted, its synchronous calls and destruction run on one pool thread.
/// A stuck call cannot be forcefully interrupted: the worker fences further
/// work immediately and reports cleanup pending until actual task retirement.
/// `Send` permits transferring ownership to that thread; no calls overlap.
pub trait LocalPresenter: Send {
    /// Configure from the exact admitted hvcC (canonical `DecoderRecord`).
    fn configure(
        &mut self,
        configuration: &Configuration,
        record: &DecoderRecord,
    ) -> Result<(), worker::Error>;
    /// Decode one admitted access unit; when `present`, also draw it.
    /// Returns only after the native decode (and present) completed.
    fn submit(&mut self, unit: &EncodedAccessUnit, present: bool) -> Result<(), worker::Error>;
    /// Install the client-rendered remote cursor between decodes and redraw
    /// the retained picture with it. Not a decode or presentation receipt.
    fn overlay(&mut self, update: &overlay::Update<'_>) -> Result<(), worker::Error>;
    /// Apply pending local resize/expose work to the retained picture, without
    /// creating a decode, visibility or freshness receipt. Called only after
    /// configuration, between commands, on this presenter's same native task.
    fn refresh(&mut self) -> Result<(), worker::Error> {
        Ok(())
    }
}

/// Only validated, bounded work enters the single native owner. The overlay
/// keeps its checked bytes because its borrowed view cannot cross the queue.
enum Command {
    Configure(Configuration, DecoderRecord),
    Unit(EncodedAccessUnit, bool),
    Overlay(Vec<u8>),
    Stop,
}
impl Command {
    fn prepare(kind: Kind, body: Vec<u8>, limits: &ProtocolLimits) -> Result<Self, Error> {
        match kind {
            Kind::ConfigureDecoder => {
                let (configuration, bytes) = Configuration::decode_decoder(&body)?;
                let guard =
                    HevcGuard::from_decoder_record(configuration.codec()?, *limits, 4, bytes)
                        .map_err(|_| Error::Protocol(worker::Error::Unsupported))?;
                let record = guard
                    .decoder_record()
                    .map_err(|_| Error::Protocol(worker::Error::Unsupported))?;
                Ok(Self::Configure(configuration, record))
            }
            Kind::Present | Kind::Decode => Ok(Self::Unit(
                worker::parse_unit(body, limits)?,
                kind == Kind::Present,
            )),
            Kind::CursorOverlay => {
                overlay::decode(&body, limits)?;
                Ok(Self::Overlay(body))
            }
            Kind::Stop => Ok(Self::Stop),
            _ => Err(Error::WorkerRefused(worker::Error::Unsupported)),
        }
    }

    fn apply(
        self,
        presenter: &mut dyn LocalPresenter,
        limits: &ProtocolLimits,
    ) -> Result<(Kind, Vec<u8>), Error> {
        match self {
            Self::Configure(configuration, record) => {
                presenter
                    .configure(&configuration, &record)
                    .map_err(Error::WorkerRefused)?;
                Ok((Kind::DecoderReady, configuration.encode_decoder(&record)?))
            }
            Self::Unit(unit, present) => {
                presenter
                    .submit(&unit, present)
                    .map_err(Error::WorkerRefused)?;
                let kind = if present {
                    Kind::Presented
                } else {
                    Kind::Decoded
                };
                Ok((kind, unit.frame().as_raw().to_be_bytes().to_vec()))
            }
            Self::Overlay(body) => {
                let update = overlay::decode(&body, limits)?;
                presenter.overlay(&update).map_err(Error::WorkerRefused)?;
                Ok((Kind::CursorOverlayApplied, Vec::new()))
            }
            Self::Stop => Err(Error::Unavailable), // handled only after native destruction
        }
    }
}

struct Request {
    command: Command,
    identity: Identity,
    deadline: Deadline,
    maximum_capacity: Option<usize>,
    reply: oneshot::Sender<Result<Record, Error>>,
}

/// The task itself owns the native object. Dropping or cancelling this owner
/// never moves its destructor back onto the authority/UI executor.
pub(super) struct Owner {
    commands: Option<SyncSender<Request>>,
    stopped: Arc<AtomicBool>,
    task: DrainedBlockingHandle<()>,
    /// A failed join is terminal, not evidence of successful native cleanup.
    join_failed: bool,
}
impl Owner {
    fn start(
        cx: &Cx,
        presenter: Box<dyn LocalPresenter>,
        limits: ProtocolLimits,
    ) -> Result<Self, Error> {
        let (commands, requests) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let task = cx
            .spawn_blocking_drained(move |child| {
                let _ambient = child.clone().set_current_restricted();
                run(&child, presenter, &requests, &stop, &limits);
            })
            .map_err(|_| Error::MissingRuntime)?;
        Ok(Self {
            commands: Some(commands),
            stopped,
            task,
            join_failed: false,
        })
    }

    pub(super) fn abort(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.commands = None;
        if !self.task.is_finished() {
            self.task.abort();
        }
    }

    fn enqueue(&mut self, request: Request) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Unavailable);
        }
        self.commands
            .as_ref()
            .ok_or(Error::Unavailable)?
            .try_send(request)
            .map_err(|_| Error::Unavailable)
    }

    async fn reap(&mut self, cx: &Cx, deadline: Deadline) -> Result<(), Error> {
        if self.join_failed {
            return Err(Error::Unavailable);
        }
        let mut previous = now(cx)?;
        // Unlike an operation wait, cleanup must also work with a cancelled
        // operation context. The exact drained join has no cancel shortcut.
        let mut joining = pin!(self.task.join());
        let mut timer = pin!(sleep_until(deadline.0));
        let result = poll_fn(|task| {
            if let Poll::Ready(result) = joining.as_mut().poll(task) {
                return Poll::Ready(Ok(result));
            }
            let current = match now(cx) {
                Ok(current) if current >= previous => current,
                Ok(_) => return Poll::Ready(Err(Error::ClockRegression)),
                Err(error) => return Poll::Ready(Err(error)),
            };
            previous = current;
            if current >= deadline.0 || timer.as_mut().poll(task).is_ready() {
                return Poll::Ready(Err(Error::ReapPending));
            }
            Poll::Pending
        })
        .await?;
        match result {
            // Queued cancellation also retires the closure's native captures
            // on the pool before join reports Cancelled.
            Ok(()) | Err(JoinError::Cancelled(_)) => Ok(()),
            Err(_) => {
                self.join_failed = true;
                Err(Error::Unavailable)
            }
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.abort();
    }
}

fn run(
    cx: &Cx,
    mut presenter: Box<dyn LocalPresenter>,
    requests: &Receiver<Request>,
    stopped: &AtomicBool,
    limits: &ProtocolLimits,
) {
    let mut configured = false;
    loop {
        if stopped.load(Ordering::Acquire) || cx.checkpoint().is_err() {
            return;
        }
        let request = match requests.recv_timeout(Duration::from_millis(5)) {
            Ok(request) => request,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if configured
                    && (stopped.load(Ordering::Acquire)
                        || cx.checkpoint().is_err()
                        || presenter.refresh().is_err())
                {
                    return;
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if stopped.load(Ordering::Acquire) || cx.checkpoint().is_err() || request.reply.is_closed()
        {
            return;
        }
        if !now(cx).is_ok_and(|current| current < request.deadline.0) {
            let _ = request.reply.send_blocking(Err(Error::Deadline));
            return;
        }
        if matches!(request.command, Command::Stop) {
            drop(presenter);
            publish(cx, request.reply, stopped, request.deadline, || {
                Record::new(Kind::Stopped, request.identity, Vec::new(), limits)
                    .map_err(Error::from)
            });
            return;
        }
        let configuring = matches!(request.command, Command::Configure(..));
        let response = request.command.apply(presenter.as_mut(), limits);
        if !publish(cx, request.reply, stopped, request.deadline, || {
            let (kind, body) = response?;
            if request
                .maximum_capacity
                .is_some_and(|maximum| body.capacity() > maximum)
            {
                return Err(Error::Protocol(worker::Error::ResourceLimit));
            }
            Record::new(kind, request.identity, body, limits).map_err(Error::from)
        }) {
            return;
        }
        configured |= configuring;
    }
}

fn publish(
    cx: &Cx,
    reply: oneshot::Sender<Result<Record, Error>>,
    stopped: &AtomicBool,
    deadline: Deadline,
    response: impl FnOnce() -> Result<Record, Error>,
) -> bool {
    if !stopped.load(Ordering::Acquire) && cx.checkpoint().is_ok() && !reply.is_closed() {
        let response = if now(cx).is_ok_and(|current| current < deadline.0) {
            response()
        } else {
            Err(Error::Deadline)
        };
        let healthy = response.is_ok();
        return reply.send_blocking(response).is_ok() && healthy;
    }
    false
}

impl Worker {
    pub(super) async fn start_local(
        cx: &Cx,
        mut launch: super::Launch,
        configuration: Configuration,
        record: &DecoderRecord,
        deadline: Deadline,
    ) -> Result<Self, Error> {
        runtime_ready(cx)?;
        let limits = configuration.limits()?;
        let body = configuration.encode_decoder(record)?;
        if launch.retirement.is_some() || launch.target.is_some() {
            return Err(Error::InvalidLaunch);
        }
        if now(cx)? >= deadline.0 {
            return Err(Error::Deadline);
        }
        let presenter = launch.local.take().ok_or(Error::InvalidLaunch)?;
        let local = Owner::start(cx, presenter, limits)?;
        let mut owner = Self {
            child: None,
            retirement: None,
            input: None,
            output: None,
            limits,
            identity: Identity {
                epoch: launch.epoch,
                sequence: 0,
            },
            role: Role::Present,
            state: State::Starting,
            exit: None,
            selected: None,
            local: Some(local),
            in_process: true,
        };
        owner
            .exchange_local(cx, Kind::ConfigureDecoder, body, None, deadline, || {
                Ok::<_, Error>(deadline.0)
            })
            .await?;
        owner.state = State::Running;
        Ok(owner)
    }

    /// Native work waits outside the executor, but preserves the same local
    /// sequencing, authority checks and drop guard as a child-pipe exchange.
    pub(super) async fn exchange_local<E: From<Error>>(
        &mut self,
        cx: &Cx,
        kind: Kind,
        body: Vec<u8>,
        maximum_capacity: Option<usize>,
        deadline: Deadline,
        live: impl FnMut() -> Result<Time, E>,
    ) -> Result<Record, E> {
        runtime_ready(cx)?;
        let identity = self.identity;
        let next = identity.sequence.checked_add(1).ok_or(Error::Unavailable)?;
        let request = Record::new(kind, identity, body, &self.limits).map_err(Error::from)?;
        let command = Command::prepare(kind, request.into_body(), &self.limits)?;
        // These are the only running requests with a nonempty reply. Refuse
        // insufficient retained credit before a native call or allocation.
        if matches!(kind, Kind::Present | Kind::Decode)
            && maximum_capacity.is_some_and(|maximum| maximum < size_of::<u64>())
        {
            return Err(Error::Protocol(worker::Error::ResourceLimit).into());
        }
        let mut guard = Exchange {
            owner: self,
            finished: false,
        };
        let owner = guard.owner.local.as_mut().ok_or(Error::Unavailable)?;
        let (reply, mut response) = oneshot::channel();
        let response = bounded(cx, deadline, live, async {
            owner.enqueue(Request {
                command,
                identity,
                deadline,
                maximum_capacity,
                reply,
            })?;
            let response = response.recv(cx).await.map_err(|error| match error {
                oneshot::RecvError::Cancelled => Error::Cancelled,
                _ => Error::PeerClosed,
            })??;
            if response.header.identity != identity
                || !super::allowed_reply(kind, response.header.kind)
            {
                return Err(Error::Protocol(worker::Error::WrongSequence));
            }
            if maximum_capacity.is_some_and(|maximum| response.header.length > maximum) {
                return Err(Error::Protocol(worker::Error::ResourceLimit));
            }
            Ok(response)
        })
        .await?;
        guard.owner.identity.sequence = next;
        if kind == Kind::Stop {
            guard.owner.state = State::Stopped;
        }
        guard.finished = true;
        Ok(response)
    }

    /// A zero status represents successful native-owner retirement, never an
    /// OS process exit. A live foreign call keeps its exact task in this owner.
    pub(super) async fn reap_local(
        &mut self,
        cx: &Cx,
        deadline: Deadline,
    ) -> Result<ExitStatus, Error> {
        self.local
            .as_mut()
            .ok_or(Error::Unavailable)?
            .reap(cx, deadline)
            .await?;
        self.local = None;
        let status = ExitStatus::from_parts(Some(0), None);
        self.exit = Some(status);
        self.state = State::Reaped;
        Ok(status)
    }
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;
