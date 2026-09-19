//! One bounded recovery request for one actual receiver generation.
use super::{
    DecodedFrame, DeliveryError, MediaBindings, ReceiveConfig, ReceivePipeline, ReceiveState,
    deadline,
};
use fr_wire::{
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    recovery_request::{self, Reason, Request},
};
use std::sync::{Arc, atomic::AtomicBool};

/// Prepared bytes are not a transport submission. Retain this offer with the
/// unchanged bytes through backpressure; authorize immediately before writing.
#[derive(Debug)]
pub struct RecoveryOffer {
    scope: Arc<AtomicBool>,
    bytes: usize,
    until: u64,
    reason: Reason,
}
impl RecoveryOffer {
    /// Original failed-chain cause, not inferred from transport availability.
    pub const fn reason(&self) -> Reason {
        self.reason
    }

    pub const fn byte_len(&self) -> usize {
        self.bytes
    }
    pub const fn send_by_micros(&self) -> u64 {
        self.until
    }
}
#[derive(Debug, Clone, Copy)]
struct Pending {
    request: Request,
    until: u64,
    sent: bool,
}
/// Construct after admission, before accepting media; keep beside the decoder.
/// A failed chain is fenced by `ReceivePipeline` before this owner emits anything.
/// It neither replaces the decoder nor grants input. After an admitted recovery,
/// construct a new owner for the new receiver scope; an old owner cannot follow it.
#[derive(Debug)]
pub struct RecoveryRequestor {
    scope: Arc<AtomicBool>,
    config: ReceiveConfig,
    binding: Binding,
    last_useful: Option<u64>,
    pending: Option<Pending>,
    last_now: Option<u64>,
    closed: bool,
}
impl RecoveryRequestor {
    pub fn new(receiver: &ReceivePipeline, binding: Binding) -> Result<Self, DeliveryError> {
        binding.validate()?;
        let (scope, config) = receiver.presentation_scope();
        receiver.check_delivery_configuration(config.limits, config.bindings, config.epoch)?;
        if binding.configuration != config.epoch.configuration
            || binding.recovery != config.epoch.recovery
        {
            return Err(DeliveryError::StaleGeneration);
        }
        Ok(Self {
            scope,
            config,
            binding,
            last_useful: None,
            pending: None,
            last_now: None,
            closed: false,
        })
    }
    /// Record only a successful completion from this exact receiver, before its
    /// receipt is consumed by presentation. No witness means unknown, not frame 0.
    pub fn observe_decoded(&mut self, frame: &DecodedFrame) -> Result<(), DeliveryError> {
        if self.closed
            || self.pending.is_some()
            || !frame.belongs_to(&self.scope)
            || frame.epoch() != self.config.epoch
            || frame.bindings() != self.config.bindings
        {
            return Err(DeliveryError::StaleGeneration);
        }
        let number = frame.descriptor().frame;
        if self.last_useful.is_some_and(|last| number < last) {
            return Err(DeliveryError::StaleGeneration);
        }
        self.last_useful = Some(number);
        Ok(())
    }
    /// Tick the real pipeline even without packets. Only reference expiry,
    /// recovery expiry and a reported decoder failure can request recovery.
    /// Protocol violations, cancellation and clock failure remain terminal.
    /// An undersized output retains the FIRST failure deadline, but sends nothing.
    pub fn offer(
        &mut self,
        receiver: &mut ReceivePipeline,
        now: u64,
        out: &mut [u8],
    ) -> Result<Option<RecoveryOffer>, DeliveryError> {
        self.check(receiver, now)?;
        let reason = match receiver.tick(now) {
            Ok(()) => return Ok(None),
            Err(DeliveryError::ReferenceExpired) => Reason::ReferenceExpired,
            Err(DeliveryError::RecoveryExpired) => Reason::RecoveryExpired,
            Err(DeliveryError::DecodeFailed) => Reason::DecodeFailed,
            Err(error) => {
                self.closed = true;
                return Err(error);
            }
        };
        if receiver.state() != ReceiveState::NeedsRecovery {
            self.closed = true;
            return Err(DeliveryError::WrongState);
        }
        if self.pending.is_none() {
            let until = match deadline(now, self.config.policy.recovery_budget_micros) {
                Ok(until) => until,
                Err(error) => {
                    self.closed = true;
                    return Err(error);
                }
            };
            self.pending = Some(Pending {
                request: Request {
                    reason,
                    last_useful_frame: self.last_useful,
                },
                until,
                sent: false,
            });
        }
        let pending = self.pending.as_ref().expect("failure installed");
        if pending.sent {
            return Ok(None);
        }
        let bytes = recovery_request::encode(
            pending.request,
            self.binding,
            self.config.limits.protocol(),
            out,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )?;
        Ok(Some(RecoveryOffer {
            scope: self.scope.clone(),
            bytes,
            until: pending.until,
            reason: pending.request.reason,
        }))
    }
    /// Checks the original receiver scope and original recovery deadline, not
    /// connection liveness. Call again after any await before transport submission.
    pub fn authorize_write(
        &mut self,
        receiver: &ReceivePipeline,
        offer: &RecoveryOffer,
        now: u64,
    ) -> Result<(), DeliveryError> {
        self.check(receiver, now)?;
        let pending = self.pending.as_ref().ok_or(DeliveryError::WrongState)?;
        if pending.sent
            || !Arc::ptr_eq(&offer.scope, &self.scope)
            || offer.until != pending.until
            || receiver.state() != ReceiveState::NeedsRecovery
        {
            return Err(DeliveryError::StaleGeneration);
        }
        Ok(())
    }
    /// Only after the bounded transport has accepted the unchanged record.
    /// No second request is emitted for this generation, even after the send.
    pub fn mark_sent(
        &mut self,
        receiver: &ReceivePipeline,
        offer: &RecoveryOffer,
        now: u64,
    ) -> Result<(), DeliveryError> {
        self.authorize_write(receiver, offer, now)?;
        self.pending.as_mut().expect("authorized offer").sent = true;
        Ok(())
    }
    /// Authorize only the next view on this ORIGINAL failed receiver, after
    /// actual request submission. Returns the original absolute deadline; fresh
    /// channels cannot widen limits, reset the budget, or follow a new scope.
    pub fn authorize_replacement(
        &mut self,
        receiver: &ReceivePipeline,
        binding: Binding,
        limits: fr_wire::MediaLimits,
        bindings: MediaBindings,
        now: u64,
    ) -> Result<u64, DeliveryError> {
        self.check(receiver, now)?;
        let pending = self.pending.as_ref().ok_or(DeliveryError::WrongState)?;
        if !pending.sent || receiver.state() != ReceiveState::NeedsRecovery {
            return Err(DeliveryError::WrongState);
        }
        let mut expected = self.binding;
        expected.recovery = expected
            .recovery
            .next()
            .ok_or(DeliveryError::StaleGeneration)?;
        if binding != expected || !bindings.all_newer_than(self.config.bindings) {
            return Err(DeliveryError::StaleGeneration);
        }
        if limits != self.config.limits {
            return Err(DeliveryError::ResourceLimit);
        }
        Ok(pending.until)
    }
    /// Includes waiting for replacement after a successful send. Polling or
    /// duplicate offers do not buy more time; the owner must service this timer.
    pub const fn next_deadline(&self) -> Option<u64> {
        match self.pending {
            Some(p) if !self.closed => Some(p.until),
            _ => None,
        }
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.pending = None;
    }
    fn check(&mut self, receiver: &ReceivePipeline, now: u64) -> Result<(), DeliveryError> {
        if self.closed {
            return Err(DeliveryError::WrongState);
        }
        if self.last_now.is_some_and(|last| now < last) {
            self.close();
            return Err(DeliveryError::ClockRegression);
        }
        self.last_now = Some(now);
        let (scope, _) = receiver.presentation_scope();
        if !Arc::ptr_eq(&self.scope, &scope) || receiver.state() == ReceiveState::Closed {
            self.close();
            return Err(DeliveryError::StaleGeneration);
        }
        if self.pending.is_some_and(|p| now >= p.until) {
            self.close();
            return Err(DeliveryError::RecoveryExpired);
        }
        Ok(())
    }
}
