//! Preserve one requested controller exchange through the original input fence.
//! A native UI can stop input immediately, without borrowing the active socket.
use super::{
    Binding, CloseRequest, ControlCloseOutcome, ControlRoutes, DRAIN_US, Error, Exchange,
    InputLeaseId, QuicConnectionState, QuicRecords, StreamRole, now,
};
use crate::quic::{ConnectionBinding, lifetime::terminal::Armed};
use asupersync::cx::Cx;
use fr_wire::closure::Reason;
use std::{
    future::Future,
    sync::{Arc, Mutex, Weak},
};

#[derive(Default)]
enum Phase {
    #[default]
    New,
    Armed,
    Ready(Result<Box<Exchange>, Error>),
    Finished,
}
#[derive(Clone, Copy)]
struct Requested {
    reason: Reason,
    until: u64,
}
#[derive(Default)]
struct State {
    phase: Phase,
    horizon: u64,
    requested: Option<Result<Requested, Error>>,
}

/// One terminal consumer, retained outside the cancelled application operation.
/// This owns no socket until its ORIGINAL transport actually closes. Dropping
/// an in-flight network operation still abandons its socket, never recovers it.
#[derive(Default)]
pub struct ControlCloseReport(Arc<Mutex<State>>);
/// Weak, one-use registration. A different connection cannot share this slot.
#[derive(Clone)]
pub struct ControlCloseRegistration(Weak<Mutex<State>>);

/// Local close intent for one actual controller. Request fixes the deadline,
/// then synchronously invokes the original input fence before returning. It
/// never sends bytes, performs native cleanup, or grants application authority.
#[derive(Clone)]
pub struct ControlCloseSignal {
    shared: Weak<Mutex<State>>,
    cleanup: Cx,
    fence: Arc<dyn Fn() + Send + Sync>,
}
impl std::fmt::Debug for ControlCloseSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ControlCloseSignal([original owner])")
    }
}
impl ControlCloseSignal {
    /// Freeze one native/UI request. Repeated requests cannot change its reason
    /// or deadline. Even failed/late requests fence the ORIGINAL input owner.
    /// This callback is bounded policy work only; it may not block or re-enter
    /// transport. Ordinary I/O observes that fence before capturing the socket.
    pub fn request(&self, reason: Reason) -> Result<bool, Error> {
        let result = (|| {
            let shared = self.shared.upgrade().ok_or(Error::Closed)?;
            let mut state = shared.lock().map_err(|_| Error::Native)?;
            if state.requested.is_some() {
                return Ok(false);
            }
            if !matches!(state.phase, Phase::Armed) {
                return Err(Error::Closed);
            }
            let request = now(&self.cleanup).and_then(|at| {
                self.cleanup.checkpoint().map_err(|_| Error::Cancelled)?;
                let until = at
                    .checked_add(DRAIN_US)
                    .ok_or(Error::Clock)?
                    .min(state.horizon);
                if at >= until {
                    return Err(Error::Expired);
                }
                Ok(Requested { reason, until })
            });
            state.requested = Some(request);
            request.map(|_| true)
        })();
        // Outside the reporting lock, before native callbacks or return.
        (self.fence)();
        result
    }
    /// The actual controller updates this only after its normal authority and
    /// deadline checks, using the MINIMUM of its existing deadlines. It is not
    /// a lease renewal. Once requested, this can never change the frozen budget.
    pub fn update_deadline(&self, horizon: u64) -> Result<(), Error> {
        let shared = self.shared.upgrade().ok_or(Error::Closed)?;
        let mut state = shared.lock().map_err(|_| Error::Native)?;
        if state.requested.is_some() {
            return Ok(());
        }
        if !matches!(state.phase, Phase::Armed) {
            return Err(Error::Closed);
        }
        state.horizon = horizon;
        Ok(())
    }
    pub fn is_requested(&self) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|shared| shared.lock().is_ok_and(|state| state.requested.is_some()))
    }
}
impl ControlCloseReport {
    pub fn registration(&self) -> ControlCloseRegistration {
        ControlCloseRegistration(Arc::downgrade(&self.0))
    }
    /// Consume custody at CALL time after ordinary service ends. No requested
    /// close means None, not success. A requested but abandoned network owner
    /// produces an explicit Closed transport failure, never a replacement socket.
    pub fn finish(self) -> impl Future<Output = Option<ControlCloseOutcome>> + use<> {
        let prepared = {
            let mut state = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let phase = std::mem::replace(&mut state.phase, Phase::Finished);
            state.requested.map(|request| {
                request.and_then(|_| match phase {
                    Phase::Ready(prepared) => prepared,
                    _ => Err(Error::Closed),
                })
            })
        };
        async move {
            match prepared? {
                Ok(exchange) => Some(exchange.run().await),
                Err(error) => Some(ControlCloseOutcome {
                    exchange: super::CloseOutcome::initial(Err(error)),
                    revocation: None,
                }),
            }
        }
    }
}

pub(in crate::quic) struct ArmedRequest {
    signal: ControlCloseSignal,
    routes: ControlRoutes,
    binding: Binding,
    lease: InputLeaseId,
}
impl Drop for ArmedRequest {
    fn drop(&mut self) {
        // Abandonment also ends this exact input lifetime before socket release.
        (self.signal.fence)();
    }
}
impl ArmedRequest {
    pub(in crate::quic) fn capture(self, records: &mut QuicRecords) {
        (self.signal.fence)();
        let Some(shared) = self.signal.shared.upgrade() else {
            return;
        };
        let requested = shared.lock().ok().and_then(|state| {
            matches!(state.phase, Phase::Armed)
                .then_some(state.requested)
                .flatten()
        });
        let Some(requested) = requested else {
            return;
        };
        let prepared = requested.and_then(|request| {
            records
                .prepare_close_exchange(
                    &self.signal.cleanup,
                    self.routes,
                    self.binding,
                    CloseRequest {
                        reason: request.reason,
                    },
                    request.until,
                    Some(self.lease),
                )
                .map(Box::new)
        });
        let mut state = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(state.phase, Phase::Armed) {
            state.phase = Phase::Ready(prepared);
        }
    }
}
impl QuicRecords {
    /// Register while this original client controller is still live. This uses
    /// the SAME one-terminal-owner slot as server reports: competing registrations
    /// cannot displace it. No input grant, OS cleanup or report is synthesized.
    ///
    /// `cleanup` is independently provisioned on the original runtime clock.
    /// `fence` stops the exact granted input owner synchronously and nonblockingly.
    /// `horizon` is its current minimum response/silence/pending-send deadline;
    /// the controller must maintain it through `ControlCloseSignal::update_deadline`.
    /// The signal may be retained by a native window, never by the remote peer.
    #[allow(clippy::too_many_arguments)]
    pub fn arm_control_close_request(
        &mut self,
        cleanup: &Cx,
        original: &ConnectionBinding,
        routes: ControlRoutes,
        binding: Binding,
        lease: InputLeaseId,
        horizon: u64,
        registration: ControlCloseRegistration,
        fence: impl Fn() + Send + Sync + 'static,
    ) -> Result<ControlCloseSignal, Error> {
        if !self.is_bound_to(original) {
            return Err(Error::WrongRoute);
        }
        if self.deferred_revocation.is_some() {
            return Err(Error::InvalidPolicy);
        }
        let at = self.check(cleanup, &mut || true)?;
        if horizon <= at {
            return Err(Error::Expired);
        }
        let native = self.native.as_ref().ok_or(Error::Closed)?;
        if native.connection().role() != StreamRole::Client
            || native.connection().state() != QuicConnectionState::Established
            || lease.as_raw() == 0
            || binding.channel == 0
            || binding.session.as_raw() == 0
            || routes.outbound.outbound == routes.inbound.outbound
            || !routes.outbound.outbound
            || routes.outbound.stream == routes.inbound.stream
        {
            return Err(Error::WrongRoute);
        }
        for route in [routes.outbound, routes.inbound] {
            if route.binding != binding.channel
                || route.messages != super::Messages::SessionControl
                || route.priority != super::Priority::Critical
                || route.maximum
                    < fr_wire::closure::CLOSED_BYTES.max(fr_wire::lease_revoked::REVOKED_BYTES)
                || !self.has_route(super::Route::Stream(route))
            {
                return Err(Error::WrongRoute);
            }
        }
        let shared = registration.0.upgrade().ok_or(Error::Closed)?;
        {
            let mut state = shared.lock().map_err(|_| Error::Native)?;
            if !matches!(state.phase, Phase::New) {
                return Err(Error::InvalidPolicy);
            }
            state.phase = Phase::Armed;
            state.horizon = horizon;
        }
        let signal = ControlCloseSignal {
            shared: registration.0,
            cleanup: cleanup.clone(),
            fence: Arc::new(fence),
        };
        self.deferred_revocation = Some(Box::new(Armed::Request(ArmedRequest {
            signal: signal.clone(),
            routes,
            binding,
            lease,
        })));
        Ok(signal)
    }
}
