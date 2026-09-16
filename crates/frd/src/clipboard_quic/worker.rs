//! The OS owner never enters a QUIC callback, even when an incoming Commit waits.
use super::{
    Arc, ChannelSession, ClientInstant, ControllerClipboard, ControllerSynchronizer, Cx, Duration,
    Error, HostInstant, IdentifierFailure, NativeClipboard, PlatformError, Received, Synchronizer,
    shared,
};
use shared::{Clock, Incoming, Shared, Sink, Turn};
use std::thread::{self, JoinHandle};

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
impl WorkerSeed {
    /// Initialization must target the locally approved OS session/display and
    /// must not read clipboard text. The resulting native owner need not be Send.
    pub fn open<N: NativeClipboard>(
        mut self,
        factory: impl FnOnce() -> Result<N, PlatformError>,
    ) -> Result<Worker<N>, Error> {
        self.clock.sample(&self.cx)?;
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
            incoming: None,
        };
        if let Err(error) = worker.clock.sample(&worker.cx) {
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
        let thread = thread::Builder::new()
            .name("fr-clipboard".into())
            .spawn(move || {
                let _ = self.shared.wake.set(thread::current());
                let mut worker = self.open(factory)?;
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
        })
    }
}
/// Owns cleanup completion, not permission. A hung foreign call cannot be killed
/// safely as a Rust thread. `finish` never waits while cleanup is still running.
pub struct WorkerTask {
    thread: Option<JoinHandle<Result<(), Error>>>,
    shared: Arc<Shared>,
}
impl WorkerTask {
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
/// Thread-confined state: one deferred incoming record, one scratch buffer and
/// the existing bounded native synchronizer. No packet history or retry queue.
pub struct Worker<N: NativeClipboard> {
    owner: Native<N>,
    shared: Arc<Shared>,
    clock: Clock,
    cx: Cx,
    scratch: Vec<u8>,
    incoming: Option<Incoming>,
}
impl<N: NativeClipboard> Worker<N> {
    pub fn close(&mut self) {
        self.shared.stop();
        self.incoming = None;
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
        self.clock.sample(&self.cx)?;
        let cx = &self.cx;
        let mut sink = Sink {
            shared: &self.shared,
            clock: &self.clock,
            cx,
        };
        // Cx was checked above; no callback/lock may hold a native operation.
        let local = || ClientInstant(shared::now(cx).unwrap_or(u64::MAX));
        let host = || HostInstant::from_micros(shared::now(cx).unwrap_or(u64::MAX));
        match &mut self.owner {
            Native::Host(n) => {
                n.poll(&mut self.scratch, &mut sink, host, &mut new_id)
                    .map_err(|_| Error::Native)?;
            }
            Native::Controller(n) => {
                n.poll(&mut self.scratch, &mut sink, local, &mut new_id)
                    .map_err(|_| Error::Native)?;
            }
        }
        if self.incoming.is_none() {
            self.incoming = self
                .shared
                .inbox
                .lock()
                .map_err(|_| Error::Poisoned)?
                .item
                .take();
        }
        if let Some(record) = &self.incoming {
            self.clock.sample(cx)?;
            let result = match &mut self.owner {
                Native::Host(n) => n
                    .receive_before(&record.bytes.0, host, record.bound)
                    .map_err(|_| Error::Native)?,
                Native::Controller(n) => n
                    .receive_before(&record.bytes.0, local, record.bound)
                    .map_err(|_| Error::Native)?,
            };
            if result != Received::Deferred {
                self.incoming = None;
                self.shared
                    .inbound_until
                    .store(0, std::sync::atomic::Ordering::Release);
                let mut inbox = self.shared.inbox.lock().map_err(|_| Error::Poisoned)?;
                match result {
                    Received::Consumed(None) => inbox.busy = false,
                    _ => inbox.result = Some(result),
                }
            }
        }
        turn.complete();
        Ok(())
    }
}
impl<N: NativeClipboard> Drop for Worker<N> {
    fn drop(&mut self) {
        self.close();
    }
}
