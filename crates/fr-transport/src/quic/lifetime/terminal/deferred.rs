//! One-use custody transfer on ordinary connection closure. This is not another
//! send mode: the consumer can only run the existing bounded terminal drain.
use super::{Binding, Drain, Error, QuicRecords, Revoked, StreamRoute};
use crate::quic::ConnectionBinding;
use asupersync::cx::Cx;
use fr_core::ids::InputLeaseId;
use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason};
use std::{
    future::Future,
    sync::{Arc, Mutex, Weak},
};

#[derive(Default)]
enum State {
    #[default]
    Unarmed,
    Armed,
    Ready(Result<Box<Drain>, Error>),
    Finished,
}

/// Single consumer of a connection's terminal report. Create before serving a
/// control-capable session, register only its actual granted lease, then finish
/// AFTER ordinary session I/O has ended. Retain this owner during native cleanup.
/// Dropping it discards any captured socket; it cannot reopen a connection.
#[derive(Default)]
pub struct RevocationReport(Arc<Mutex<State>>);

/// Weak, one-use registration, not input authority or a transport handle. Copies
/// cannot register two connections or keep a dropped reporting consumer alive.
#[derive(Clone)]
pub struct RevocationRegistration(Weak<Mutex<State>>);

impl RevocationReport {
    pub fn registration(&self) -> RevocationRegistration {
        RevocationRegistration(Arc::downgrade(&self.0))
    }
    /// Freeze the captured outcome at CALL time. No grant/registration means
    /// `None`, not a delivered report. An armed connection which was abandoned
    /// before closure cannot be recovered; its outcome is `Some(Err(Closed))`.
    /// The drain's fixed deadline was set at closure and is never restarted.
    pub fn finish(self) -> impl Future<Output = Option<Result<(), Error>>> + use<> {
        let prepared = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match std::mem::replace(&mut *state, State::Finished) {
                State::Unarmed => None,
                State::Ready(result) => Some(result),
                State::Armed | State::Finished => Some(Err(Error::Closed)),
            }
        };
        async move {
            match prepared {
                None => None,
                Some(Err(error)) => Some(Err(error)),
                Some(Ok(drain)) => Some(drain.run().await),
            }
        }
    }
}

pub(in crate::quic) struct Armed {
    cleanup: Cx,
    route: StreamRoute,
    binding: Binding,
    lease: InputLeaseId,
    registration: RevocationRegistration,
    fence: Option<Box<dyn Fn() -> Reason + Send + Sync>>,
}
impl Drop for Armed {
    fn drop(&mut self) {
        // This field precedes the socket: abandonment also fences the lease
        // before releasing transport custody. No I/O is performed in Drop.
        if let Some(fence) = self.fence.take() {
            let _ = fence();
        }
    }
}
impl QuicRecords {
    /// Register terminal reporting while the original connection is still live.
    /// `cleanup` is the caller's independent cleanup context on the original
    /// timer driver, provisioned before session cancellation. Never un-cancel a
    /// session Cx. Its own cancellation and the immutable ingress guard apply.
    ///
    /// On close, `fence` MUST synchronously fence this exact input lease before
    /// returning its content-free reason. It must not block or re-enter this
    /// transport. Stages are always Fenced/Unknown: no native result is inferred.
    /// Normal I/O errors and explicit close transfer at most ONE socket, subject
    /// to the existing native-backlog refusal. Dropping an in-flight I/O future
    /// still abandons that socket. The returned report never grants input.
    #[allow(clippy::too_many_arguments)]
    pub fn arm_revocation_report(
        &mut self,
        cleanup: &Cx,
        original: &ConnectionBinding,
        route: StreamRoute,
        binding: Binding,
        lease: InputLeaseId,
        registration: RevocationRegistration,
        fence: impl Fn() -> Reason + Send + Sync + 'static,
    ) -> Result<(), Error> {
        if !self.is_bound_to(original) {
            return Err(Error::WrongRoute);
        }
        if self.deferred_revocation.is_some() {
            return Err(Error::InvalidPolicy);
        }
        self.check(cleanup, &mut || true)?;
        self.revocation_route(route, binding)?;
        if binding.session.as_raw() == 0 || lease.as_raw() == 0 {
            return Err(Error::Malformed);
        }
        let shared = registration.0.upgrade().ok_or(Error::Closed)?;
        {
            let mut state = shared.lock().map_err(|_| Error::Native)?;
            if !matches!(*state, State::Unarmed) {
                return Err(Error::InvalidPolicy);
            }
            *state = State::Armed;
        }
        self.deferred_revocation = Some(Box::new(Armed {
            cleanup: cleanup.clone(),
            route,
            binding,
            lease,
            registration,
            fence: Some(Box::new(fence)),
        }));
        Ok(())
    }

    pub(in crate::quic) fn capture_revocation(&mut self) {
        let Some(mut armed) = self.deferred_revocation.take() else {
            return;
        };
        // No reporting lock while fencing authority or inspecting native state.
        // Nothing below drives native I/O or dispatches application records.
        let reason = armed.fence.take().expect("one fence per registration")();
        let Some(shared) = armed.registration.0.upgrade() else {
            return;
        };
        let prepared = self
            .prepare_revocation(
                &armed.cleanup,
                armed.route,
                armed.binding,
                Revoked {
                    lease: armed.lease,
                    reason,
                    cleanup: CleanupStage::Fenced,
                    effects: EffectStage::Unknown,
                },
            )
            .map(Box::new);
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(*state, State::Armed) {
            *state = State::Ready(prepared);
        }
        // A consumer that already finished cannot recover this socket. Any
        // unclaimed prepared report drops, never restores ordinary I/O.
    }
}
