//! Best-effort notification of an abrupt end by the original client owner.
//! An explicit terminal exchange has first custody; this fallback never takes
//! its socket, replays application data, or constructs a remote cleanup report.
use super::{Error, QuicRecords, now, packet::Packet};
use crate::quic::{ClientCloseReason, ConnectionBinding, PeerClose};
use asupersync::{cx::Cx, net::quic_native::StreamRole};
use std::{
    future::Future,
    sync::{Arc, Mutex, Weak},
};

#[derive(Default)]
enum State {
    #[default]
    New,
    Armed,
    Ready(Result<Box<Packet>, Error>),
    Finished,
}

/// Retained by the connection supervisor until the application has stopped.
/// This owns no socket until the original transport synchronously closes.
/// Dropping the consumer never spawns work or sends a packet from Drop.
#[derive(Default)]
pub struct ClientAbortReport(Arc<Mutex<State>>);

/// A weak, one-use slot, tied to one exact native client connection.
#[derive(Clone)]
pub struct ClientAbortRegistration(Weak<Mutex<State>>);

impl ClientAbortReport {
    pub fn registration(&self) -> ClientAbortRegistration {
        ClientAbortRegistration(Arc::downgrade(&self.0))
    }

    /// Consume captured custody at CALL time. Await before cancelling the
    /// independently provisioned connection/cleanup context. A normal explicit
    /// exchange or an already-closed peer leaves no fallback socket and returns
    /// None. Ok means only that the protected close was written to the socket:
    /// no peer ACK, native cleanup, input release or effect receipt is inferred.
    /// Delayed polling consumes the original closure-time 250 ms budget.
    pub fn finish(self) -> impl Future<Output = Option<Result<(), Error>>> + use<> {
        let prepared = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, State::Finished) {
                State::Ready(prepared) => Some(prepared),
                State::New | State::Armed | State::Finished => None,
            }
        };
        async move {
            match prepared? {
                Ok(packet) => Some(packet.run().await),
                Err(error) => Some(Err(error)),
            }
        }
    }
}

pub(in crate::quic) struct Armed {
    cleanup: Cx,
    registration: ClientAbortRegistration,
    fence: Option<Box<dyn Fn() + Send + Sync>>,
}
impl Drop for Armed {
    fn drop(&mut self) {
        // Abandoning a still-live owner also fences it, without async I/O.
        if let Some(fence) = self.fence.take() {
            fence();
        }
    }
}

impl QuicRecords {
    /// Register before application traffic on this exact established client.
    /// `cleanup` comes from the connection supervisor on the original clock;
    /// `fence` synchronously stops this viewer, including any later control
    /// promotion. Both are fixed before service and cannot be replaced on error.
    /// The immutable destination/security gate must already be installed.
    ///
    /// This lower-priority slot does not displace a normal `CloseRequest` or an
    /// already-registered terminal owner. Only a socket left by those paths can
    /// be captured, and only a content-free native close can subsequently run.
    pub fn arm_client_abort(
        &mut self,
        cleanup: &Cx,
        original: &ConnectionBinding,
        registration: ClientAbortRegistration,
        fence: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), Error> {
        if !self.is_bound_to(original) {
            return Err(Error::WrongRoute);
        }
        if self.client_abort.is_some() {
            return Err(Error::InvalidPolicy);
        }
        let native = self.native.as_ref().ok_or(Error::Closed)?;
        if native.connection().role() != StreamRole::Client {
            return Err(Error::WrongRoute);
        }
        if self.terminal_lifetime_check.is_none()
            || self.senders.iter().any(|sender| sender.bytes != 0)
            || self.read_bytes != 0
            || native.connection().inner().datagrams_sent() != 0
            || native
                .connection()
                .inner()
                .pending_outbound_datagram_count()
                != 0
            || self.senders.iter().any(|sender| {
                native
                    .connection()
                    .inner()
                    .streams()
                    .stream(sender.route.stream)
                    .is_ok_and(|stream| stream.send_offset != 0)
            })
        {
            return Err(Error::InvalidPolicy);
        }
        self.check(cleanup, &mut || true)?;
        let shared = registration.0.upgrade().ok_or(Error::Closed)?;
        {
            let mut state = shared.lock().map_err(|_| Error::Native)?;
            if !matches!(*state, State::New) {
                return Err(Error::InvalidPolicy);
            }
            *state = State::Armed;
        }
        self.client_abort = Some(Box::new(Armed {
            cleanup: cleanup.clone(),
            registration,
            fence: Some(Box::new(fence)),
        }));
        Ok(())
    }

    pub(in crate::quic) fn capture_client_abort(&mut self, reason: ClientCloseReason) {
        let Some(mut armed) = self.client_abort.take() else {
            return;
        };
        if self.native.is_none() {
            // An explicit exchange already took custody after its own fence.
            // It may deliberately use the still-live application context for
            // observation closing; cancelling it here would abort that exchange.
            armed.fence = None;
            return;
        }
        let started = now(&armed.cleanup);
        // Original application admission ends before custody can be captured.
        if let Some(fence) = armed.fence.take() {
            fence();
        }
        let Some(shared) = armed.registration.0.upgrade() else {
            return;
        };
        // A successful explicit exchange owns the socket already. A peer's
        // terminal state is not a new local failure and receives no response.
        if self.native.as_ref().is_none_or(|native| {
            native.connection().state()
                != asupersync::net::quic_native::QuicConnectionState::Established
        }) {
            return;
        }
        let prepared = started.and_then(|started| {
            self.prepare_terminal_packet(&armed.cleanup, started, PeerClose::Client(reason).code())
        });
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, State::Armed) {
            *state = State::Ready(prepared.map(Box::new));
        }
    }
}
