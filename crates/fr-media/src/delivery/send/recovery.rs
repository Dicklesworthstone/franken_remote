//! Per-subscription admission and fencing of bound recovery requests.
use super::{DeliveryError, OfferOrigin, SendCache, SendError, deadline};
use fr_wire::{
    decoder::Binding,
    input::{InputDelivery, InputDirection},
    recovery_request::{self, Request},
};

pub(super) struct PendingRecovery {
    pub(super) request: Request,
    pub(super) binding: Binding,
    pub(super) until: u64,
}
/// One accepted request. Not cloneable: a shared encoder coalescer consumes it
/// once. Numeric frame/generation equality cannot substitute for cache ownership.
#[derive(Debug)]
pub struct RecoveryDemand {
    origin: OfferOrigin,
    request: Request,
    until: u64,
}
impl RecoveryDemand {
    pub const fn request(&self) -> Request {
        self.request
    }
    pub const fn deadline_micros(&self) -> u64 {
        self.until
    }
    pub(crate) fn check(&self, cache: &SendCache, now: u64) -> Result<(), DeliveryError> {
        if cache.closed
            || !cache.needs_recovery
            || !std::sync::Arc::ptr_eq(&self.origin.owner, &cache.owner)
            || self.origin.epoch != cache.epoch
            || cache
                .recovery_request
                .as_ref()
                .is_none_or(|p| p.until != self.until || p.request != self.request)
        {
            return Err(DeliveryError::StaleGeneration);
        }
        if now >= self.until {
            return Err(DeliveryError::RecoveryExpired);
        }
        Ok(())
    }
}
#[derive(Debug)]
pub enum RecoveryDisposition {
    Accepted(RecoveryDemand),
    Coalesced,
}
impl SendCache {
    /// The authenticated control route supplies this subscription's INSTALLED
    /// full view binding, never a peer-proposed tuple. The session must still
    /// fence input/readiness and admit fresh channel bindings before replacement.
    /// Parsing, future-frame checks and coalescing precede any new recovery work.
    pub fn request_recovery(
        &mut self,
        bytes: &[u8],
        binding: Binding,
        now: u64,
    ) -> Result<RecoveryDisposition, SendError> {
        self.check_clock(now)?;
        if self.closed {
            return Err(SendError::Closed);
        }
        self.check_recovery_deadline(now)?;
        if binding.configuration != self.epoch.configuration
            || binding.recovery != self.epoch.recovery
        {
            return Err(DeliveryError::StaleGeneration.into());
        }
        let request = recovery_request::decode(
            bytes,
            binding,
            self.limits.protocol(),
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )?;
        if request
            .last_useful_frame
            .is_some_and(|frame| self.last_inserted.is_none_or(|last| frame > last))
        {
            return Err(SendError::InvalidSequence);
        }
        if let Some(pending) = &self.recovery_request {
            if pending.binding != binding {
                return Err(DeliveryError::StaleGeneration.into());
            }
            return Ok(RecoveryDisposition::Coalesced);
        }
        let until = match deadline(now, self.policy.recovery_horizon_micros) {
            Ok(until) => until,
            Err(error) => {
                self.close();
                return Err(error.into());
            }
        };
        // Stop old originals, repairs and already-prepared PacketOffers before
        // handing any demand to a shared capture owner. Other caches are untouched.
        self.clear();
        self.needs_recovery = true;
        self.recovery_request = Some(PendingRecovery {
            request,
            binding,
            until,
        });
        Ok(RecoveryDisposition::Accepted(RecoveryDemand {
            origin: OfferOrigin {
                owner: self.owner.clone(),
                epoch: self.epoch,
            },
            request,
            until,
        }))
    }
    pub(super) fn check_recovery_deadline(&mut self, now: u64) -> Result<(), SendError> {
        if self.recovery_request.as_ref().is_some_and(|p| now >= p.until) {
            self.close();
            return Err(DeliveryError::RecoveryExpired.into());
        }
        Ok(())
    }
}
