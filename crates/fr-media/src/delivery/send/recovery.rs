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
        // A chronically failing subscription is refused BEFORE issuing another
        // unique demand to the shared encoder. Replacement recognizes this epoch
        // as already charged; duplicate requests and fresh bindings cannot refund it.
        self.admit_recovery(now)?;
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
        if self
            .recovery_request
            .as_ref()
            .is_some_and(|p| now >= p.until)
        {
            self.close();
            return Err(DeliveryError::RecoveryExpired.into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{IdrCoalescer, MediaBindings, MediaEpoch, SendPolicy};
    use fr_core::{ids::*, limits::ProtocolLimits};
    use fr_wire::{Channel, MediaLimits, recovery_request::Reason};

    fn cache() -> SendCache {
        SendCache::new(
            MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
            MediaBindings::new(1, 2, 3, 4).unwrap(),
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            SendPolicy {
                max_recoveries_per_window: 2,
                recovery_window_micros: 1_000_000,
                ..SendPolicy::default()
            },
        )
        .unwrap()
    }
    fn binding(cache: &SendCache) -> Binding {
        Binding {
            parent: fr_wire::negotiation::ControlBinding {
                id: 1000,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: cache.epoch.configuration,
            recovery: cache.epoch.recovery,
            viewport: ViewportMappingGeneration::INITIAL,
        }
    }
    fn record(binding: Binding, frame: Option<u64>) -> Vec<u8> {
        let mut bytes = vec![0; recovery_request::REQUEST_BYTES];
        let n = recovery_request::encode(
            Request {
                reason: Reason::DecodeFailed,
                last_useful_frame: frame,
            },
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        assert_eq!(n, bytes.len());
        bytes
    }
    fn request(cache: &mut SendCache, now: u64) -> Result<RecoveryDisposition, SendError> {
        let binding = binding(cache);
        cache.request_recovery(&record(binding, None), binding, now)
    }
    fn accepted(cache: &mut SendCache, now: u64) -> RecoveryDemand {
        match request(cache, now).unwrap() {
            RecoveryDisposition::Accepted(demand) => demand,
            RecoveryDisposition::Coalesced => panic!("expected a new generation"),
        }
    }
    fn replace(cache: &mut SendCache, now: u64) {
        let base = cache.bindings.for_channel(Channel::Control) + 1;
        cache
            .replace(
                MediaEpoch {
                    recovery: cache.epoch.recovery.next().unwrap(),
                    ..cache.epoch
                },
                MediaBindings::new(base, base + 1, base + 2, base + 3).unwrap(),
                now,
            )
            .unwrap();
    }
    #[test]
    fn exhausted_viewer_cannot_issue_another_shared_encoder_demand() {
        let mut failed = cache();
        let mut healthy = cache();
        let mut encoder = IdrCoalescer::new(100_000).unwrap();
        for now in [0, 100_000] {
            let demand = accepted(&mut failed, now);
            encoder.queue(&failed, demand, now).unwrap();
            assert!(encoder.take(now).unwrap().is_some());
            replace(&mut failed, now + 1);
        }
        assert!(matches!(
            request(&mut failed, 200_000),
            Err(SendError::RecoveryLimitExceeded)
        ));
        assert_eq!(encoder.next_deadline(), None);
        assert_eq!(encoder.take(200_000).unwrap(), None);
        assert!(matches!(
            request(&mut failed, 2_000_000),
            Err(SendError::Closed)
        ));
        let demand = accepted(&mut healthy, 2_000_000);
        encoder.queue(&healthy, demand, 2_000_000).unwrap();
        assert!(encoder.take(2_000_000).unwrap().is_some());
        assert_eq!(healthy.epoch.recovery, RecoveryGeneration::INITIAL);
    }
    #[test]
    fn duplicate_flood_and_replacement_charge_exactly_once_per_generation() {
        let mut cache = cache();
        let first = accepted(&mut cache, 0);
        let deadline = first.deadline_micros();
        for now in 1..10_000 {
            assert!(matches!(
                request(&mut cache, now),
                Ok(RecoveryDisposition::Coalesced)
            ));
            assert_eq!(cache.next_deadline(), Some(deadline));
        }
        assert_eq!(cache.recoveries.iter().flatten().count(), 1);
        replace(&mut cache, 10_000);
        assert_eq!(cache.recoveries.iter().flatten().count(), 1);
        let second = accepted(&mut cache, 10_001);
        assert_eq!(cache.recoveries.iter().flatten().count(), 2);
        replace(&mut cache, 10_002);
        assert!(first.check(&cache, 10_002).is_err());
        assert!(second.check(&cache, 10_002).is_err());
        assert!(matches!(
            request(&mut cache, 10_003),
            Err(SendError::RecoveryLimitExceeded)
        ));
    }
    #[test]
    fn invalid_requests_do_not_spend_allowance_or_fence_a_healthy_generation() {
        let mut cache = cache();
        drop(accepted(&mut cache, 0));
        replace(&mut cache, 1);
        let installed = binding(&cache);
        let mut stale = installed;
        stale.parent.remote_session = RemoteSessionId::from_raw(90);
        for now in 2..100 {
            assert!(
                cache
                    .request_recovery(&record(stale, None), installed, now)
                    .is_err()
            );
            assert_eq!(
                cache
                    .request_recovery(&record(installed, Some(1)), installed, now)
                    .unwrap_err(),
                SendError::InvalidSequence
            );
            assert!(cache.request_recovery(&[0; 8], installed, now).is_err());
            assert!(!cache.needs_recovery());
            assert_eq!(cache.recoveries.iter().flatten().count(), 1);
        }
        drop(accepted(&mut cache, 100));
        replace(&mut cache, 101);
        assert!(!cache.needs_recovery());
    }
    #[test]
    fn dropping_a_demand_does_not_refund_the_subscription_allowance() {
        let mut cache = cache();
        for now in [0, 10] {
            drop(accepted(&mut cache, now));
            replace(&mut cache, now + 1);
        }
        assert!(matches!(
            request(&mut cache, 20),
            Err(SendError::RecoveryLimitExceeded)
        ));
    }
    #[test]
    fn request_window_uses_original_admission_time_not_replacement_time() {
        let mut cache = cache();
        drop(accepted(&mut cache, 0));
        replace(&mut cache, 100);
        drop(accepted(&mut cache, 200));
        replace(&mut cache, 300);
        drop(accepted(&mut cache, 1_000_000));
        assert_eq!(cache.recoveries.iter().flatten().count(), 2);
        replace(&mut cache, 1_000_001);
        assert!(matches!(
            request(&mut cache, 1_000_002),
            Err(SendError::RecoveryLimitExceeded)
        ));
    }
}
