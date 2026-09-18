//! One-slot, nonblocking handoff to the original controller's disk owner.
//!
//! No async runtime is created. Filesystem work and cleanup run on a dedicated
//! thread; revoke and mailbox admission never wait for that thread. A stuck OS
//! call cannot be safely killed as a Rust thread, and is not claimed cancelled.
use crate::{
    receive::{DropDirectory, Expected, MAX_CHUNK_BYTES},
    session::{self, Binding, HostReceiver, Permission, Policy, Progress, VerifiedOffer},
};
use asupersync::{
    atp::{object::ContentId, safety::validate_portable_path_component},
    cx::Cx,
    time::TimerDriverHandle,
};
use fr_core::{input_submission::InputSession, time::HostInstant};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, OnceLock, TryLockError},
    thread::{self, JoinHandle, Thread},
    time::Duration,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Busy,
    WrongBinding,
    UnknownEffect,
    Closed,
    InvalidName,
    InvalidChunk,
    Allocation,
    SequenceExhausted,
    Poisoned,
    Spawn,
    Clock,
    Cancelled,
    Panicked,
    Session(session::Error),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Begun(Progress),
    Written(Progress),
    Published(session::Receipt),
    Cancelled,
}
/// Correlates local handoff only; the number is never an authorization token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Receipt {
    pub sequence: u64,
    pub result: Result<Completion, session::Error>,
}
struct Payload(Vec<u8>);
impl Drop for Payload {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
enum Command {
    Begin {
        id: u64,
        name: String,
        size: u64,
        expected: Expected,
    },
    Chunk {
        id: u64,
        offset: u64,
        bytes: Payload,
    },
    Atp {
        id: u64,
        bytes: Payload,
    },
    Complete(u64),
    Cancel(u64),
}
struct Work {
    sequence: u64,
    command: Command,
}
enum Slot {
    Idle,
    Queued(Work),
    Executing,
    Complete(Receipt),
}
struct Inbox {
    next: u64,
    slot: Slot,
}
struct Shared {
    inbox: Mutex<Inbox>,
    permission: Permission,
    cx: Cx,
    clock: TimerDriverHandle,
    wake: OnceLock<Thread>,
}
impl Shared {
    // Every native-stage clock callback also checks parent cancellation. In
    // particular, cancellation during fsync is fenced at the final rename gate.
    fn sample(&self) -> HostInstant {
        if self.cx.checkpoint().is_err() {
            self.permission.revoke();
        }
        HostInstant::from_micros(self.clock.now().as_nanos() / 1000)
    }
    fn wake(&self) {
        if let Some(thread) = self.wake.get() {
            thread.unpark();
        }
    }
    fn stop(&self) {
        self.permission.revoke();
        self.wake();
    }
}

/// One pending command INCLUDING executing work and its uncollected receipt.
/// The caller cannot enqueue a second 64-KiB chunk while the first is on disk.
/// No method performs file I/O or blocks waiting for a mutex or worker.
pub struct Mailbox {
    shared: Arc<Shared>,
    binding: Binding,
}
impl std::fmt::Debug for Mailbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileMailbox([original worker])")
    }
}
impl Mailbox {
    pub fn binding(&self) -> Binding {
        self.binding
    }
    /// Success means queued only. Collect the separately staged actual result.
    pub fn begin(&self, id: u64, name: &str, size: u64, expected: ContentId) -> Result<u64, Error> {
        self.begin_verified(VerifiedOffer {
            binding: self.binding(),
            id,
            name,
            size,
            expected: Expected::Content(expected),
        })
    }
    pub(crate) fn begin_verified(&self, offer: VerifiedOffer<'_>) -> Result<u64, Error> {
        if offer.binding != self.binding() {
            return Err(Error::WrongBinding);
        }
        if offer.name.len() > 255
            || offer.name.starts_with(".fr-part-")
            || validate_portable_path_component(offer.name).is_err()
        {
            return Err(Error::InvalidName);
        }
        self.submit(|| {
            let mut owned = String::new();
            owned
                .try_reserve_exact(offer.name.len())
                .map_err(|_| Error::Allocation)?;
            owned.push_str(offer.name);
            Ok(Command::Begin {
                id: offer.id,
                name: owned,
                size: offer.size,
                expected: offer.expected,
            })
        })
    }
    /// Cancellation fences admission even before the worker's next timer turn.
    pub fn is_closed(&self) -> bool {
        if self.shared.cx.is_cancel_requested() {
            self.shared.stop();
        }
        !self.shared.permission.is_approved()
    }
    pub fn write_chunk(&self, id: u64, offset: u64, bytes: &[u8]) -> Result<u64, Error> {
        if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
            return Err(Error::InvalidChunk);
        }
        self.submit(|| {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(bytes.len())
                .map_err(|_| Error::Allocation)?;
            owned.extend_from_slice(bytes);
            Ok(Command::Chunk {
                id,
                offset,
                bytes: Payload(owned),
            })
        })
    }
    /// Queue one complete, bounded upstream ATP frame. Parsing and disk work
    /// stay on the worker; accepting the handoff is not a publication receipt.
    pub fn atp_record(&self, id: u64, bytes: &[u8]) -> Result<u64, Error> {
        if bytes.is_empty() || bytes.len() > crate::atp::MAX_FRAME_BYTES {
            return Err(Error::InvalidChunk);
        }
        self.submit(|| {
            let mut owned = Vec::new();
            owned
                .try_reserve_exact(bytes.len())
                .map_err(|_| Error::Allocation)?;
            owned.extend_from_slice(bytes);
            Ok(Command::Atp {
                id,
                bytes: Payload(owned),
            })
        })
    }
    pub fn complete(&self, id: u64) -> Result<u64, Error> {
        self.submit(|| Ok(Command::Complete(id)))
    }
    pub fn cancel(&self, id: u64) -> Result<u64, Error> {
        self.submit(|| Ok(Command::Cancel(id)))
    }
    pub fn stop(&self) {
        self.shared.stop();
    }
    /// Completed effects remain readable AFTER revocation or worker completion.
    /// Never replace an actual Published receipt with a later cancellation.
    pub fn take_receipt(&self) -> Result<Option<Receipt>, Error> {
        let mut inbox = self
            .shared
            .inbox
            .try_lock()
            .map_err(|error| lock_error(&error))?;
        if let Slot::Complete(receipt) = inbox.slot {
            inbox.slot = Slot::Idle;
            Ok(Some(receipt))
        } else {
            Ok(None)
        }
    }
    fn submit(&self, make: impl FnOnce() -> Result<Command, Error>) -> Result<u64, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let mut inbox = self
            .shared
            .inbox
            .try_lock()
            .map_err(|error| lock_error(&error))?;
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if !matches!(inbox.slot, Slot::Idle) {
            return Err(Error::Busy);
        }
        let next = inbox.next.checked_add(1).ok_or(Error::SequenceExhausted)?;
        // Copy only after acquiring the ONE free slot. Even concurrent callers
        // cannot allocate an unbounded set of pending records behind a slow disk.
        let work = Work {
            sequence: inbox.next,
            command: make()?,
        };
        inbox.next = next;
        let sequence = work.sequence;
        inbox.slot = Slot::Queued(work);
        drop(inbox);
        self.shared.wake();
        Ok(sequence)
    }
}
impl Drop for Mailbox {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Cleanup completion is separate from permission. Dropping requests stop but
/// never joins a stuck OS call on the input or transport thread.
pub struct Task {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Result<(), Error>>>,
}
impl std::fmt::Debug for Task {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FileTask([disk owner])")
    }
}
impl Task {
    pub fn stop(&self) {
        self.shared.stop();
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn try_finish(&mut self) -> Option<Result<(), Error>> {
        if !self.is_finished() {
            return None;
        }
        self.thread
            .take()
            .map(|t| t.join().unwrap_or(Err(Error::Panicked)))
    }
}
impl Drop for Task {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Start a fresh worker under an already admitted, separately approved original
/// controller. Opening the local drop directory is setup work, not peer input.
pub fn spawn(
    cx: Cx,
    input: &InputSession,
    root: DropDirectory,
    permission: Permission,
    policy: Policy,
) -> Result<(Mailbox, Task), Error> {
    cx.checkpoint().map_err(|_| Error::Cancelled)?;
    let clock = cx.timer_driver().ok_or(Error::Clock)?;
    let now = HostInstant::from_micros(clock.now().as_nanos() / 1000);
    let receiver =
        HostReceiver::new(input, root, permission.clone(), policy, now).map_err(Error::Session)?;
    let binding = receiver.binding();
    let shared = Arc::new(Shared {
        inbox: Mutex::new(Inbox {
            next: 1,
            slot: Slot::Idle,
        }),
        permission,
        cx,
        clock,
        wake: OnceLock::new(),
    });
    let running = shared.clone();
    let thread = thread::Builder::new()
        .name("fr-files".into())
        .spawn(move || run(receiver, &running))
        .map_err(|_| {
            shared.stop();
            Error::Spawn
        })?;
    Ok((
        Mailbox {
            shared: shared.clone(),
            binding,
        },
        Task {
            shared,
            thread: Some(thread),
        },
    ))
}
fn lock_error<T>(error: &TryLockError<T>) -> Error {
    match error {
        TryLockError::WouldBlock => Error::Busy,
        TryLockError::Poisoned(_) => Error::Poisoned,
    }
}
fn execute(
    receiver: &mut HostReceiver,
    shared: &Shared,
    command: Command,
) -> Result<Completion, session::Error> {
    let binding = receiver.binding();
    match command {
        Command::Begin {
            id,
            name,
            size,
            expected,
        } => receiver
            .begin_verified(
                VerifiedOffer {
                    binding,
                    id,
                    name: &name,
                    size,
                    expected,
                },
                || shared.sample(),
            )
            .map(Completion::Begun),
        Command::Chunk { id, offset, bytes } => receiver
            .write(binding, id, offset, &bytes.0, || shared.sample())
            .map(Completion::Written),
        Command::Atp { id, bytes } => {
            let record = match crate::atp::ObjectRecord::decode(&bytes.0) {
                Ok(record) => record,
                Err(error) => {
                    let _ = receiver.close();
                    return Err(error);
                }
            };
            match record.data() {
                Some((offset, data)) => receiver
                    .write(binding, id, offset, data, || shared.sample())
                    .map(Completion::Written),
                None => receiver
                    .complete(binding, id, || shared.sample())
                    .map(Completion::Published),
            }
        }
        Command::Complete(id) => receiver
            .complete(binding, id, || shared.sample())
            .map(Completion::Published),
        Command::Cancel(id) => receiver.cancel(binding, id).map(|()| Completion::Cancelled),
    }
}
fn run(mut receiver: HostReceiver, shared: &Shared) -> Result<(), Error> {
    let _ = shared.wake.set(thread::current());
    loop {
        // Maintenance occurs even when Idle or a result is uncollected. Traffic
        // and mailbox polling cannot keep a dead controller's file alive.
        if let Err(error) = receiver.service(shared.sample()) {
            shared.stop();
            let mut inbox = shared.inbox.lock().map_err(|_| Error::Poisoned)?;
            if matches!(inbox.slot, Slot::Queued(_))
                && let Slot::Queued(work) = std::mem::replace(&mut inbox.slot, Slot::Idle)
            {
                inbox.slot = Slot::Complete(Receipt {
                    sequence: work.sequence,
                    result: Err(error),
                });
            }
            return Err(Error::Session(error));
        }
        let work = {
            let mut inbox = shared.inbox.lock().map_err(|_| Error::Poisoned)?;
            if matches!(inbox.slot, Slot::Queued(_)) {
                match std::mem::replace(&mut inbox.slot, Slot::Executing) {
                    Slot::Queued(work) => Some(work),
                    _ => None,
                }
            } else {
                None
            }
        };
        if let Some(work) = work {
            let receipt = Receipt {
                sequence: work.sequence,
                result: catch_unwind(AssertUnwindSafe(|| {
                    execute(&mut receiver, shared, work.command)
                }))
                .unwrap_or_else(|_| {
                    // A panic after rename may have committed. Fence files,
                    // retain an explicit uncertain effect, and never retry.
                    shared.stop();
                    Err(session::Error::UnknownEffect)
                }),
            };
            let mut inbox = shared.inbox.lock().map_err(|_| Error::Poisoned)?;
            inbox.slot = Slot::Complete(receipt);
        }
        thread::park_timeout(Duration::from_millis(10));
    }
}
