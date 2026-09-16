//! Fixed record slots. No native code or user callback runs under these locks.
use super::{
    Admission, Arc, ClientInstant, ControllerTransport, Cx, Disposition, Egress, Error,
    HostInstant, Received, RecordSink, Transport, TransportFailure,
};
use std::sync::{
    Mutex, OnceLock, TryLockError,
    atomic::{AtomicU64, Ordering},
};

pub(super) const HANDOFF_US: u64 = 3_000_000;

#[derive(Clone)]
pub(super) enum Clock {
    Host(Transport),
    Controller(ControllerTransport),
}
impl Clock {
    pub(super) fn sample(&self, cx: &Cx) -> Result<(u64, HostInstant), Error> {
        cx.checkpoint().map_err(|_| Error::Cancelled)?;
        let local = now(cx)?;
        let host = match self {
            Self::Host(gate) => {
                let at = HostInstant::from_micros(local);
                gate.check(at).map_err(Error::Session)?;
                at
            }
            Self::Controller(gate) => gate
                .sample(ClientInstant(local))
                .map_err(Error::Controller)?,
        };
        Ok((local, host))
    }
}
pub(super) fn now(cx: &Cx) -> Result<u64, Error> {
    Ok(cx.timer_driver().ok_or(Error::Clock)?.now().as_nanos() / 1000)
}

pub(super) struct Bytes(pub(super) Vec<u8>);
impl Bytes {
    pub(super) fn copy(bytes: &[u8], maximum: usize) -> Result<Self, Error> {
        if bytes.len() > maximum {
            return Err(Error::Limit);
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(bytes.len())
            .map_err(|_| Error::Allocation)?;
        owned.extend_from_slice(bytes);
        Ok(Self(owned))
    }
}
impl Drop for Bytes {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
pub(super) struct Incoming {
    pub(super) bytes: Bytes,
    pub(super) bound: HostInstant,
}
pub(super) struct Outgoing {
    pub(super) bytes: Bytes,
    pub(super) permit: Egress,
    pub(super) until: u64,
}
/// `busy` covers queued, executing, deferred AND uncollected results, not just
/// whether a Vec is currently inside the mutex. Taking cannot expand capacity.
#[derive(Default)]
pub(super) struct Inbox {
    pub(super) busy: bool,
    pub(super) item: Option<Incoming>,
    pub(super) result: Option<Received>,
}
#[derive(Default)]
pub(super) struct Outbox {
    pub(super) busy: bool,
    pub(super) item: Option<Outgoing>,
}
pub(super) struct Shared {
    pub(super) gate: Transport,
    pub(super) maximum: usize,
    pub(super) inbox: Mutex<Inbox>,
    pub(super) outbox: Mutex<Outbox>,
    pub(super) inbound_until: AtomicU64,
    pub(super) wake: OnceLock<std::thread::Thread>,
}
impl Shared {
    pub(super) fn stop(&self) {
        self.gate.close();
        self.wake();
        // Only internal metadata is touched. A running worker retains its own
        // one bounded record until it returns; its final authority is fenced now.
        if let Ok(mut i) = self.inbox.try_lock() {
            i.item = None;
        }
        if let Ok(mut o) = self.outbox.try_lock() {
            o.item = None;
        }
    }
    pub(super) fn wake(&self) {
        if let Some(worker) = self.wake.get() {
            worker.unpark();
        }
    }
    pub(super) fn accept(
        &self,
        bytes: &[u8],
        local: u64,
        host: HostInstant,
    ) -> Result<Disposition, ()> {
        let mut inbox = match self.inbox.try_lock() {
            Ok(v) => v,
            Err(TryLockError::WouldBlock) => return Ok(Disposition::Blocked),
            Err(TryLockError::Poisoned(_)) => {
                self.gate.close();
                return Err(());
            }
        };
        if inbox.busy {
            return Ok(Disposition::Blocked);
        }
        if !self.gate.is_open() {
            return Err(());
        }
        let owned = Bytes::copy(bytes, self.maximum).map_err(|_| ())?;
        let until = local.checked_add(HANDOFF_US).ok_or(())?;
        let bound = host
            .checked_add(fr_core::time::HostDuration::from_micros(HANDOFF_US))
            .ok_or(())?;
        *inbox = Inbox {
            busy: true,
            item: Some(Incoming {
                bytes: owned,
                bound,
            }),
            result: None,
        };
        self.inbound_until.store(until, Ordering::Release);
        drop(inbox);
        self.wake();
        Ok(Disposition::Consumed)
    }
}
/// Only the trusted wire owner can supply a permit. Unchecked record enqueue is
/// deliberately refused rather than silently turning a synchronous sink async.
pub(super) struct Sink<'a> {
    pub(super) shared: &'a Shared,
    pub(super) clock: &'a Clock,
    pub(super) cx: &'a Cx,
}
impl RecordSink for Sink<'_> {
    fn try_send(&mut self, _: &[u8]) -> Result<Admission, TransportFailure> {
        Err(TransportFailure)
    }
    fn try_send_checked(
        &mut self,
        bytes: &[u8],
        permit: Egress,
    ) -> Result<Admission, TransportFailure> {
        let (local, host) = self.clock.sample(self.cx).map_err(|_| TransportFailure)?;
        permit.check_operation(host).map_err(|_| TransportFailure)?;
        if permit.context() != self.shared.gate.context() {
            return Err(TransportFailure);
        }
        // Conversion is captured ONCE before entering the queue. Better clock
        // correlation, scheduling delay and backpressure cannot move this later.
        let remaining = permit
            .deadline()
            .as_micros()
            .checked_sub(host.as_micros())
            .ok_or(TransportFailure)?;
        let until = local.checked_add(remaining).ok_or(TransportFailure)?;
        let mut outbox = match self.shared.outbox.try_lock() {
            Ok(v) => v,
            Err(TryLockError::WouldBlock) => return Ok(Admission::Backpressure),
            Err(TryLockError::Poisoned(_)) => return Err(TransportFailure),
        };
        if outbox.busy {
            return Ok(Admission::Backpressure);
        }
        let bytes = Bytes::copy(bytes, self.shared.maximum).map_err(|_| TransportFailure)?;
        outbox.item = Some(Outgoing {
            bytes,
            permit,
            until,
        });
        outbox.busy = true;
        Ok(Admission::Accepted)
    }
}
/// Closes both logical sides even when user code unwinds before the native
/// owner's own external-call guard is installed. No native call in Drop here.
pub(super) struct Turn {
    shared: Arc<Shared>,
    completed: bool,
}
impl Turn {
    pub(super) fn new(shared: &Arc<Shared>) -> Self {
        Self {
            shared: shared.clone(),
            completed: false,
        }
    }
    pub(super) fn complete(&mut self) {
        self.completed = true;
    }
}
impl Drop for Turn {
    fn drop(&mut self) {
        if !self.completed {
            self.shared.stop();
        }
    }
}
