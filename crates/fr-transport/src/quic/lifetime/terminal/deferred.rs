//! One-use custody transfer on ordinary connection closure. This is not another
//! send mode: the consumer can only run a bounded terminal drain or close packet.
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

mod abrupt;
use abrupt::ClosePacket;

// Exactly one terminal owner. A close packet is not a delivered FRD0 report;
// its successful flush deliberately retains the report's Backpressure result.
enum Prepared {
    Record(Box<Drain>),
    Close(Box<ClosePacket>),
}
impl Prepared {
    fn with_closed(mut self, report: Closed) -> Result<Self, Error> {
        match &mut self {
            Self::Record(drain) => {
                drain.byte_len = Report::Closed(report).encode(drain.binding, &mut drain.bytes)?;
            }
            Self::Close(close) => close.replace_report(report)?,
        }
        Ok(self)
    }
    async fn run(self) -> Result<(), Error> {
        match self {
            Self::Record(drain) => drain.run().await,
            Self::Close(close) => close.run().await,
        }
    }
}

#[derive(Default)]
enum State {
    #[default]
    Unarmed,
    Armed,
    Superseded,
    Ready(Result<Prepared, Error>),
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
    /// Native backlog permits only one protected close packet, not payload
    /// draining. Even if flushed, the undelivered report returns Backpressure.
    pub fn finish(self) -> impl Future<Output = Option<Result<(), Error>>> + use<> {
        finish(self.take())
    }
    fn take(self) -> Option<Result<Prepared, Error>> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::mem::replace(&mut *state, State::Finished) {
            State::Unarmed | State::Superseded => None,
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
/// nor cleanup success. Its exact original slot may transfer to a granted
/// lease's revocation owner; it cannot replace a lease-revocation consumer.
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
    /// A validated transfer to the original session's lease-revocation owner
    /// also returns `None`: this superseded consumer cannot report twice.
    /// A native-backlog fallback carries only this final reason, never guessed
    /// cleanup/effect stages, and cannot return a successful report receipt.
    pub fn finish(self, report: Closed) -> impl Future<Output = Option<Result<(), Error>>> + use<> {
        let prepared = self
            .0
            .take()
            .map(|result| result.and_then(|prepared| prepared.with_closed(report)));
        finish(prepared)
    }
}

async fn finish(prepared: Option<Result<Prepared, Error>>) -> Option<Result<(), Error>> {
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

pub(in crate::quic) enum Armed {
    Report(ArmedReport),
    Request(super::exchange::deferred::ArmedRequest),
}

pub(in crate::quic) struct ArmedReport {
    cleanup: Cx,
    route: StreamRoute,
    binding: Binding,
    registration: RevocationRegistration,
    promoted_from: Option<ClosedRegistration>,
    fence: Option<Fence>,
}
impl Drop for ArmedReport {
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
    /// Normal I/O errors and explicit close transfer at most ONE socket. Native
    /// backlog still refuses the report; only a protected `CONNECTION_CLOSE` may
    /// be flushed instead, without application payload or delivery confirmation.
    /// Dropping an in-flight I/O future still abandons that socket. The returned
    /// report never grants input.
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
    /// This also covers a control-capable session before any lease exists.
    /// Its original registration must transfer to lease-specific reporting
    /// with `promote_closed_report` when the actual granted owner is installed.
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

    /// Transfer an armed pre-grant closure slot to its actual granted lease.
    /// The caller must already have validated the input owner's authority and
    /// connection custody; this operation itself never grants or renews input.
    ///
    /// Both the original connection proof and the exact closure registration,
    /// route and full session binding must match. The old consumer must still
    /// be armed and the new consumer unused. Refusal leaves its fence armed;
    /// consumed session/input owners must still perform their ordinary teardown.
    /// A successful transfer retires the old consumer without running its
    /// observation fence. Only the replacement lease fence can capture a socket.
    ///
    /// The original cleanup context, clock, immutable security gate and close
    /// deadline policy are retained. No second socket, detached drain or fresh
    /// cleanup budget is created by promotion.
    #[allow(clippy::too_many_arguments)]
    pub fn promote_closed_report(
        &mut self,
        original: &ConnectionBinding,
        route: StreamRoute,
        binding: Binding,
        lease: InputLeaseId,
        previous: &ClosedRegistration,
        registration: RevocationRegistration,
        fence: impl Fn() -> Reason + Send + Sync + 'static,
    ) -> Result<(), Error> {
        if !self.is_bound_to(original) {
            return Err(Error::WrongRoute);
        }
        if lease.as_raw() == 0 {
            return Err(Error::Malformed);
        }
        let Some(Armed::Report(armed)) = self.deferred_revocation.as_deref() else {
            return Err(Error::InvalidPolicy);
        };
        if !matches!(armed.fence.as_ref(), Some(Fence::Closed(_))) {
            return Err(Error::InvalidPolicy);
        }
        if armed.route != route
            || armed.binding != binding
            || !Weak::ptr_eq(&armed.registration.0, &previous.0.0)
        {
            return Err(Error::WrongRoute);
        }
        let cleanup = armed.cleanup.clone();
        let previous_shared = previous.0.0.upgrade().ok_or(Error::Closed)?;
        let shared = registration.0.upgrade().ok_or(Error::Closed)?;
        if Arc::ptr_eq(&previous_shared, &shared) {
            return Err(Error::InvalidPolicy);
        }
        // These checks may close/fence on cancellation or lifetime loss. Never
        // hold either reporting lock while a transport check can run a fence.
        self.check(&cleanup, &mut || true)?;
        self.terminal_route(route, binding, REVOKED_BYTES)?;
        let mut previous_state = previous_shared.lock().map_err(|_| Error::Native)?;
        let mut state = shared.lock().map_err(|_| Error::Native)?;
        if !matches!(*previous_state, State::Armed) || !matches!(*state, State::Unarmed) {
            return Err(Error::InvalidPolicy);
        }
        let Some(Armed::Report(armed)) = self.deferred_revocation.as_deref_mut() else {
            return Err(Error::InvalidPolicy);
        };
        // All fallible custody checks have completed under the two consumer
        // locks. Retire the old callback only inside this one-way handoff.
        *previous_state = State::Superseded;
        *state = State::Armed;
        armed.registration = registration;
        armed.promoted_from = Some(previous.clone());
        let retired = armed.fence.replace(Fence::Revocation {
            lease,
            stop: Box::new(fence),
        });
        drop(state);
        drop(previous_state);
        drop(retired);
        Ok(())
    }

    /// Confirm the exact already-armed lease slot when its original input owner
    /// moves from grant publication into steady-state service. This never arms
    /// a slot, replaces a callback, renews a budget or recovers a closed socket.
    /// `previous` must name the original pre-grant slot if one was promoted.
    #[allow(clippy::too_many_arguments)]
    pub fn confirm_revocation_report(
        &mut self,
        original: &ConnectionBinding,
        route: StreamRoute,
        binding: Binding,
        lease: InputLeaseId,
        previous: Option<&ClosedRegistration>,
        registration: &RevocationRegistration,
    ) -> Result<(), Error> {
        if !self.is_bound_to(original) {
            return Err(Error::WrongRoute);
        }
        let Some(Armed::Report(armed)) = self.deferred_revocation.as_deref() else {
            return Err(Error::InvalidPolicy);
        };
        let Some(Fence::Revocation { lease: granted, .. }) = &armed.fence else {
            return Err(Error::InvalidPolicy);
        };
        let same_previous = match (armed.promoted_from.as_ref(), previous) {
            (Some(owned), Some(expected)) => Weak::ptr_eq(&owned.0.0, &expected.0.0),
            (None, None) => true,
            _ => false,
        };
        if *granted != lease
            || armed.route != route
            || armed.binding != binding
            || !same_previous
            || !Weak::ptr_eq(&armed.registration.0, &registration.0)
        {
            return Err(Error::WrongRoute);
        }
        let cleanup = armed.cleanup.clone();
        let shared = registration.0.upgrade().ok_or(Error::Closed)?;
        self.check(&cleanup, &mut || true)?;
        self.terminal_route(route, binding, REVOKED_BYTES)?;
        let state = shared.lock().map_err(|_| Error::Native)?;
        if !matches!(*state, State::Armed) {
            return Err(Error::InvalidPolicy);
        }
        Ok(())
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
        self.deferred_revocation = Some(Box::new(Armed::Report(ArmedReport {
            cleanup: cleanup.clone(),
            route,
            binding,
            registration,
            promoted_from: None,
            fence: Some(fence),
        })));
        Ok(())
    }

    fn prepare_registered_report(
        &mut self,
        cleanup: &Cx,
        route: StreamRoute,
        binding: Binding,
        report: Report,
    ) -> Result<Prepared, Error> {
        // Freeze the fallback deadline before attempting the ordinary drain.
        // A failed preparation cannot restart it or publish malformed metadata.
        let started = super::now(cleanup)?;
        let code = abrupt::report_code(&report, binding)?;
        match self.prepare_report(cleanup, route, binding, report) {
            Ok(drain) => Ok(Prepared::Record(Box::new(drain))),
            Err(Error::Backpressure) => self
                .prepare_close_packet(cleanup, route, binding, started, code)
                .map(|close| Prepared::Close(Box::new(close))),
            Err(error) => Err(error),
        }
    }

    pub(in crate::quic) fn capture_revocation(&mut self) {
        let Some(armed) = self.deferred_revocation.take() else {
            return;
        };
        let mut armed = match *armed {
            Armed::Request(request) => return request.capture(self),
            Armed::Report(report) => report,
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
        let prepared =
            self.prepare_registered_report(&armed.cleanup, armed.route, armed.binding, report);
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
