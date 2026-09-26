//! One-use custody transfer on ordinary connection closure. This is not another
//! send mode: the consumer can only run the existing bounded terminal drain.
use super::{Binding, Closed, Drain, Error, QuicRecords, Report, Revoked, StreamRoute};
use crate::quic::ConnectionBinding;
use asupersync::cx::Cx;
use fr_core::ids::InputLeaseId;
use fr_wire::{
    closure::{CLOSED_BYTES, Cleanup, ClosedReason, OutstandingEffects},
    lease_revoked::{CleanupStage, EffectStage, REVOKED_BYTES, Reason},
};
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
        finish(self.take())
    }
    fn take(self) -> Option<Result<Box<Drain>, Error>> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *state, State::Finished) {
            State::Unarmed => None,
            State::Ready(result) => Some(result),
            State::Armed | State::Finished => Some(Err(Error::Closed)),
        }
    }
}

/// One session-closure consumer, distinct from lease-revocation reporting. Its
/// owner supplies the actual outcome only AFTER fencing and ordered teardown.
/// Capturing a socket never invents completed cleanup or zero outstanding effects.
/// There is at most one terminal registration of either kind per connection.
#[derive(Default)]
pub struct ClosedReport(RevocationReport);

/// Weak one-use session-closure registration. It grants neither normal traffic
/// nor cleanup success, and cannot replace an armed lease-revocation consumer.
#[derive(Clone)]
pub struct ClosedRegistration(RevocationRegistration);

impl ClosedReport {
    pub fn registration(&self) -> ClosedRegistration {
        ClosedRegistration(self.0.registration())
    }
    /// Freeze the terminal outcome at CALL time, not on first poll. `report`
    /// must describe this session's original owners; transport cannot derive
    /// native cleanup or receipt counts. The original 250-ms closure deadline
    /// includes time already spent cleaning up. Late completion cannot renew it.
    /// No registered/captured socket means no report, never a reconnect or retry.
    pub fn finish(self, report: Closed) -> impl Future<Output = Option<Result<(), Error>>> + use<> {
        let prepared = self.0.take().map(|result| {
            result.and_then(|mut drain| {
                drain.byte_len = Report::Closed(report).encode(drain.binding, &mut drain.bytes)?;
                Ok(drain)
            })
        });
        finish(prepared)
    }
}

async fn finish(prepared: Option<Result<Box<Drain>, Error>>) -> Option<Result<(), Error>> {
    match prepared {
        None => None,
        Some(Err(error)) => Some(Err(error)),
        Some(Ok(drain)) => Some(drain.run().await),
    }
}

enum Fence {
    Revocation {
        lease: InputLeaseId,
        stop: Box<dyn Fn() -> Reason + Send + Sync>,
    },
    Closed(Box<dyn Fn() + Send + Sync>),
}
impl Fence {
    const fn byte_len(&self) -> usize {
        match self {
            Self::Revocation { .. } => REVOKED_BYTES,
            Self::Closed(_) => CLOSED_BYTES,
        }
    }
    fn run(self) -> Report {
        match self {
            Self::Revocation { lease, stop } => Report::Revoked(Revoked {
                lease,
                reason: stop(),
                cleanup: CleanupStage::Fenced,
                effects: EffectStage::Unknown,
            }),
            Self::Closed(stop) => {
                stop();
                // Private preparation only: no I/O occurs here. ClosedReport's
                // consumer MUST replace this with its final typed outcome before
                // the detached drain becomes runnable. An abandoned owner drops
                // the socket, never publishes a guessed reason or success stage.
                Report::Closed(Closed {
                    reason: ClosedReason::HostFailure,
                    cleanup: Cleanup::Unconfirmed,
                    effects: OutstandingEffects::Unknown,
                })
            }
        }
    }
}

pub(in crate::quic) struct Armed {
    cleanup: Cx,
    route: StreamRoute,
    binding: Binding,
    registration: RevocationRegistration,
    fence: Option<Fence>,
}
impl Drop for Armed {
    fn drop(&mut self) {
        // This field precedes the socket: abandonment also fences the lease or
        // observation before releasing custody. No I/O is performed in Drop.
        if let Some(fence) = self.fence.take() {
            let _ = fence.run();
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
        if lease.as_raw() == 0 {
            return Err(Error::Malformed);
        }
        self.arm_terminal(
            cleanup,
            original,
            route,
            binding,
            registration,
            Fence::Revocation {
                lease,
                stop: Box::new(fence),
            },
        )
    }

    /// Register ONE observation-session closure consumer before ordinary I/O
    /// can close its socket. The owner must fence this exact observation in the
    /// bounded callback, then finish the report AFTER its ordered teardown.
    /// `cleanup` has the same independent-clock/security requirements as the
    /// lease-specific registration. No callback executes native I/O or awaits.
    /// A control-capable owner must retain lease-specific reporting instead.
    #[allow(clippy::too_many_arguments)]
    pub fn arm_closed_report(
        &mut self,
        cleanup: &Cx,
        original: &ConnectionBinding,
        route: StreamRoute,
        binding: Binding,
        registration: ClosedRegistration,
        fence: impl Fn() + Send + Sync + 'static,
    ) -> Result<(), Error> {
        self.arm_terminal(
            cleanup,
            original,
            route,
            binding,
            registration.0,
            Fence::Closed(Box::new(fence)),
        )
    }

    fn arm_terminal(
        &mut self,
        cleanup: &Cx,
        original: &ConnectionBinding,
        route: StreamRoute,
        binding: Binding,
        registration: RevocationRegistration,
        fence: Fence,
    ) -> Result<(), Error> {
        if !self.is_bound_to(original) {
            return Err(Error::WrongRoute);
        }
        if self.deferred_revocation.is_some() {
            return Err(Error::InvalidPolicy);
        }
        self.check(cleanup, &mut || true)?;
        self.terminal_route(route, binding, fence.byte_len())?;
        if binding.channel == 0 || binding.session.as_raw() == 0 {
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
            registration,
            fence: Some(fence),
        }));
        Ok(())
    }

    pub(in crate::quic) fn capture_revocation(&mut self) {
        let Some(mut armed) = self.deferred_revocation.take() else {
            return;
        };
        // No reporting lock while fencing authority or inspecting native state.
        // Nothing below drives native I/O or dispatches application records.
        let report = armed
            .fence
            .take()
            .expect("one fence per registration")
            .run();
        let Some(shared) = armed.registration.0.upgrade() else {
            return;
        };
        let prepared = self
            .prepare_report(&armed.cleanup, armed.route, armed.binding, report)
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
