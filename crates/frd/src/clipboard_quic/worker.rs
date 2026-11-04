//! The OS owner never enters a QUIC callback, even when an incoming Commit waits.
use super::{
    Arc, ChannelSession, ClientInstant, ControllerClipboard, ControllerSynchronizer, Cx, Duration,
    Error, HostInstant, IdentifierFailure, NativeClipboard, PlatformError, Received, Synchronizer,
    shared,
};
use fr_wire::clipboard::session::synchronize::SyncError;
use shared::{Clock, Shared, Sink, Turn};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    thread::{self, JoinHandle},
};

pub(super) enum Session {
    Host(ChannelSession),
    Controller(ControllerClipboard),
}
/// Send this one-use seed to the actual interactive worker, then call `open`.
/// Alternatively `spawn` creates a foreign-call thread, not an async runtime.
pub struct WorkerSeed {
    pub(super) session: Option<Session>,
    pub(super) shared: Arc<Shared>,
    pub(super) clock: Clock,
    pub(super) cx: Cx,
}
impl Drop for WorkerSeed {
    fn drop(&mut self) {
        if self.session.is_some() {
            self.shared.stop();
        }
    }
}
/// Independent terminal stop for the original native worker; not a grant or
/// proof of cleanup. Stopping before `open` prevents the factory from running.
#[derive(Clone)]
pub struct WorkerControl {
    shared: Arc<Shared>,
}
impl WorkerControl {
    pub fn stop(&self) {
        self.shared.stop();
    }
}
impl std::fmt::Debug for WorkerControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClipboardWorkerControl([original worker fence])")
    }
}
impl WorkerSeed {
    pub fn control(&self) -> WorkerControl {
        WorkerControl {
            shared: self.shared.clone(),
        }
    }

    /// Initialization must target the locally approved OS session/display and
    /// must not read clipboard text. The resulting native owner need not be Send.
    pub fn open<N: NativeClipboard>(
        mut self,
        factory: impl FnOnce() -> Result<N, PlatformError>,
    ) -> Result<Worker<N>, Error> {
        self.clock.sample_maintenance(&self.cx)?;
        ready(&self.clock, &self.cx)?;
        let native = factory().map_err(Error::NativeSetup)?;
        let owner = match self.session.take().ok_or(Error::Closed)? {
            Session::Host(session) => Native::Host(Synchronizer::new(session, native)),
            Session::Controller(session) => Native::Controller(session.into_native(native)),
        };
        let mut worker = Worker {
            owner,
            shared: self.shared.clone(),
            clock: self.clock.clone(),
            cx: self.cx.clone(),
            scratch: vec![
                0;
                self.shared.maximum.min(
                    fr_core::clipboard::MAX_CHUNK_BYTES + fr_wire::clipboard::CHUNK_OVERHEAD
                )
            ],
        };
        if let Err(error) = ready(&worker.clock, &worker.cx) {
            worker.close();
            return Err(error);
        }
        Ok(worker)
    }
    /// `factory` runs entirely on the new native thread, after the original
    /// authority check. `new_id` supplies qualified randomness, never text/IDs
    /// received from the peer. Dropping the handle stops admission, not the OS.
    pub fn spawn<N, F, I>(self, factory: F, mut new_id: I) -> Result<WorkerTask, Error>
    where
        N: NativeClipboard + 'static,
        F: FnOnce() -> Result<N, PlatformError> + Send + 'static,
        I: FnMut() -> Result<u128, IdentifierFailure> + Send + 'static,
    {
        let shared = self.shared.clone();
        let ready = Arc::new(AtomicBool::new(false));
        let opened = ready.clone();
        let thread = thread::Builder::new()
            .name("fr-clipboard".into())
            .spawn(move || {
                let _ = self.shared.wake.set(thread::current());
                let mut worker = self.open(factory)?;
                opened.store(true, Ordering::Release);
                while worker.shared.gate.is_open() {
                    worker.step(&mut new_id)?;
                    thread::park_timeout(Duration::from_millis(1));
                }
                worker.close();
                Ok(())
            })
            .map_err(|_| {
                shared.stop();
                Error::Thread
            })?;
        Ok(WorkerTask {
            thread: Some(thread),
            shared,
            ready,
        })
    }
}
/// Owns cleanup completion, not permission. A hung foreign call cannot be killed
/// safely as a Rust thread. `finish` never waits while cleanup is still running.
pub struct WorkerTask {
    thread: Option<JoinHandle<Result<(), Error>>>,
    shared: Arc<Shared>,
    ready: Arc<AtomicBool>,
}
impl WorkerTask {
    /// Positive evidence from THIS original worker, after native factory and
    /// post-setup authority checks completed. Thread creation is not readiness.
    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.shared.stop();
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn finish(&mut self) -> Option<Result<(), Error>> {
        if !self.is_finished() {
            return None;
        }
        self.thread
            .take()
            .map(|t| t.join().unwrap_or(Err(Error::Panicked)))
    }
}
impl Drop for WorkerTask {
    fn drop(&mut self) {
        self.stop();
    }
}
enum Native<N: NativeClipboard> {
    Host(Synchronizer<N>),
    Controller(ControllerSynchronizer<N>),
}
/// Thread-confined native owner and one scratch buffer. Queued and explicitly
/// deferred ingress stay in the shared slot; only execution removes its record.
pub struct Worker<N: NativeClipboard> {
    owner: Native<N>,
    shared: Arc<Shared>,
    clock: Clock,
    cx: Cx,
    scratch: Vec<u8>,
}
impl<N: NativeClipboard> Worker<N> {
    pub fn close(&mut self) {
        self.shared.stop();
        self.scratch.fill(0);
        match &mut self.owner {
            Native::Host(n) => n.close(),
            Native::Controller(n) => n.close(),
        }
    }
    /// Call during traffic AND silence on the interactive worker. Returns only
    /// typed status; native error details and clipboard bytes do not cross threads.
    pub fn step(
        &mut self,
        mut new_id: impl FnMut() -> Result<u128, IdentifierFailure>,
    ) -> Result<(), Error> {
        let mut turn = Turn::new(&self.shared);
        let (local, host) = self.clock.sample_maintenance(&self.cx)?;
        self.check_handoffs(local, host)?;
        if self.finish_expired(local, host)? {
            turn.complete();
            return Ok(());
        }
        ready(&self.clock, &self.cx)?;
        let cx = self.cx.clone();
        let mut sink = Sink {
            shared: &self.shared,
            clock: &self.clock,
            cx: &cx,
        };
        // Cx was checked above; no callback/lock may hold a native operation.
        // The synchronizer also runs maintenance while paused, expiring its
        // private buffers and cancelling expired reads without observing new
        // text, publishing or sending. Its submission checks own readiness.
        let local = || ClientInstant(shared::now(&cx).unwrap_or(u64::MAX));
        let host = || HostInstant::from_micros(shared::now(&cx).unwrap_or(u64::MAX));
        let polled = match &mut self.owner {
            Native::Host(n) => n
                .poll(&mut self.scratch, &mut sink, host, &mut new_id)
                .map_err(|error| native_error(&error)),
            Native::Controller(n) => n
                .poll(&mut self.scratch, &mut sink, local, &mut new_id)
                .map_err(|error| controller_error(&error)),
        };
        if paused(&polled)? {
            turn.complete();
            return Ok(());
        }
        // Native polling can consume time. Recheck the same ingress bounds
        // before transferring this slot into native receive ownership.
        let (local_now, host_now) = self.clock.sample_maintenance(&self.cx)?;
        self.check_handoffs(local_now, host_now)?;
        if self.finish_expired(local_now, host_now)? {
            turn.complete();
            return Ok(());
        }
        if !ready(&self.clock, &self.cx)? {
            turn.complete();
            return Ok(());
        }
        let record = self
            .shared
            .inbox
            .lock()
            .map_err(|_| Error::Poisoned)?
            .item
            .take();
        if let Some(record) = record {
            let result = match &mut self.owner {
                Native::Host(n) => n
                    .receive_before(&record.bytes.0, host, record.bound)
                    .map_err(|error| native_error(&error)),
                Native::Controller(n) => n
                    .receive_before(&record.bytes.0, local, record.bound)
                    .map_err(|error| controller_error(&error)),
            }?;
            if result == Received::Deferred {
                // Only the synchronizer's explicit unsubmitted result returns
                // the exact record to the safe queue, with its original bound.
                self.shared
                    .inbox
                    .lock()
                    .map_err(|_| Error::Poisoned)?
                    .item = Some(record);
            } else {
                self.finish_incoming(result)?;
            }
        }
        turn.complete();
        Ok(())
    }

    fn finish_expired(&mut self, local: u64, host: HostInstant) -> Result<bool, Error> {
        let expired = self.shared.inbox.lock().map_err(|_| Error::Poisoned)?.expired;
        let Some(expired) = expired else {
            return Ok(false);
        };
        // Keep the marker in its busy slot until this release-only owner turn
        // completes. The bridge can distinguish it from executing native work.
        let result = match &mut self.owner {
            Native::Host(n) => n
                .expire_receive(expired, host)
                .map_err(|error| native_error(&error)),
            Native::Controller(n) => n
                .expire_receive(expired, ClientInstant(local))
                .map_err(|error| controller_error(&error)),
        }?;
        if result
            == Received::Refused(fr_wire::clipboard::session::SessionError::Clipboard(
                fr_core::clipboard::Error::Disabled,
            ))
        {
            return Err(Error::SwitchChanged);
        }
        self.finish_incoming(result)?;
        Ok(true)
    }
    fn finish_incoming(&self, result: Received) -> Result<(), Error> {
        let mut inbox = self.shared.inbox.lock().map_err(|_| Error::Poisoned)?;
        inbox.item = None;
        inbox.expired = None;
        match result {
            Received::Consumed(None) => inbox.busy = false,
            _ => inbox.result = Some(result),
        }
        // Publish completion and clear its deadline under the same lock. A
        // delayed collector must retain any actual native publication receipt.
        self.shared.inbound_until.store(0, Ordering::Release);
        Ok(())
    }

    /// Pausing native calls never pauses original transfer time. The bridge and
    /// worker use the same exact slot and can release only unsubmitted records.
    fn check_handoffs(&self, local: u64, host: HostInstant) -> Result<(), Error> {
        if !self.shared.gate.is_open() {
            return Err(Error::Closed);
        }
        self.shared.expire_incoming(local, host)?;
        match self.shared.outbox.try_lock() {
            Ok(mut outbox) => {
                if let Some(item) = &outbox.item
                    && let Err(reason) = item.check_operation(local, host)
                {
                    if !item.can_discard(reason, &self.clock, &self.shared.gate.limits()) {
                        return Err(reason);
                    }
                    // No byte left this slot. The existing wire owner emits its
                    // Cancel before a replacement; never replay the old cursor.
                    outbox.item = None;
                    outbox.busy = false;
                }
            }
            Err(std::sync::TryLockError::WouldBlock) => {}
            Err(std::sync::TryLockError::Poisoned(_)) => return Err(Error::Poisoned),
        }
        Ok(())
    }
}

/// A paused original grant remains a live owner. Native initialization is
/// allowed because its factory contract prohibits observing clipboard text;
/// text reads and remote publication require readiness. An existing watcher
/// can service bounded metadata and already-published local selections.
fn ready(clock: &Clock, cx: &Cx) -> Result<bool, Error> {
    match clock.sample(cx) {
        Ok(_) => Ok(true),
        Err(error) if error.is_suspended() => Ok(false),
        Err(error) => Err(error),
    }
}

fn paused<T>(result: &Result<T, Error>) -> Result<bool, Error> {
    match result {
        Ok(_) => Ok(false),
        Err(error) if error.is_suspended() => Ok(true),
        Err(error) => Err(*error),
    }
}

fn native_error<E>(error: &SyncError<E>) -> Error {
    match error {
        SyncError::Session(error) => Error::Session(*error),
        _ => Error::Native,
    }
}

fn controller_error<E>(error: &fr_client::clipboard::NativeError<E>) -> Error {
    match error {
        fr_client::clipboard::NativeError::Authority(error) => Error::Controller(*error),
        fr_client::clipboard::NativeError::Synchronizer(error) => native_error(error),
    }
}

impl<N: NativeClipboard> Drop for Worker<N> {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests;
