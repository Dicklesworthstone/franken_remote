//! Fixed-space shared-encoder IDR admission; never a global pipeline reset.
use super::{DeliveryError, RecoveryDemand, SendCache, deadline};

#[derive(Debug, Clone, Copy)]
struct Pending {
    ready: u64,
    until: u64,
}
/// Keep one coalescer for the shared encoder lifetime, not one per viewer or
/// recovery generation. Accepted requests occupy ONE slot and retain the first
/// cohort's deadline. New generations and cancellation do not refill rate credit.
#[derive(Debug)]
pub struct IdrCoalescer {
    interval: u64,
    next_allowed: u64,
    pending: Option<Pending>,
    last_now: Option<u64>,
    closed: bool,
}
impl IdrCoalescer {
    /// At most ten admissions per second, at least one per second when queued.
    /// This is an enqueue-rate bound, not a claim about native encoder latency.
    pub fn new(minimum_interval_micros: u64) -> Result<Self, DeliveryError> {
        if !(100_000..=1_000_000).contains(&minimum_interval_micros) {
            return Err(DeliveryError::InvalidPolicy);
        }
        Ok(Self {
            interval: minimum_interval_micros,
            next_allowed: 0,
            pending: None,
            last_now: None,
            closed: false,
        })
    }
    /// Only a demand issued by this actual, still-fenced sender can enter the
    /// shared queue. A foreign cache, expired request or replaced epoch refuses.
    pub fn queue(
        &mut self,
        cache: &SendCache,
        demand: RecoveryDemand,
        now: u64,
    ) -> Result<(), DeliveryError> {
        self.check_clock(now)?;
        demand.check(cache, now)?;
        let until = demand.deadline_micros();
        // An idle encoder owner may receive a new request before polling take().
        // Retire an already expired cohort instead of poisoning the new viewer's
        // valid demand with its deadline. This never changes next_allowed, and
        // invalid demands cannot retire work because ownership was checked first.
        if self.pending.is_some_and(|pending| now >= pending.until) {
            self.pending = None;
        }
        // Consume the unique proof even when another viewer already queued work.
        drop(demand);
        match &mut self.pending {
            Some(p) => p.until = p.until.min(until),
            None => {
                self.pending = Some(Pending {
                    ready: now.max(self.next_allowed),
                    until,
                });
            }
        }
        Ok(())
    }
    /// Call immediately when enqueueing a bounded force-IDR encoder command,
    /// never merely while preparing it. Returns its original maximum deadline.
    /// Charge even an unsuccessful enqueue: retries cannot bypass the rate bound.
    /// The caller must not reset a healthy viewer's bindings or source lifetime.
    pub fn take(&mut self, now: u64) -> Result<Option<u64>, DeliveryError> {
        self.check_clock(now)?;
        if self.pending.is_some_and(|p| now >= p.until) {
            self.pending = None;
            return Err(DeliveryError::RecoveryExpired);
        }
        self.admit(now, None)
    }
    /// Admit a locally authorized late-join cohort on the SAME encoder rate
    /// allowance as loss recovery. The caller owns its bounded join slots and
    /// immutable deadline; this method stores no join request or viewer state.
    /// Calling again, cancelling a join or expiring a cohort never refills credit.
    /// A coincident recovery demand is satisfied by this one IDR, with the earlier
    /// deadline. This is scheduling only, not permission to observe a source.
    pub fn take_for_join(&mut self, now: u64, until: u64) -> Result<Option<u64>, DeliveryError> {
        self.check_clock(now)?;
        if now >= until {
            return Err(DeliveryError::RecoveryExpired);
        }
        if self.pending.is_some_and(|p| now >= p.until) {
            self.pending = None;
        }
        self.admit(now, Some(until))
    }
    fn admit(&mut self, now: u64, join_until: Option<u64>) -> Result<Option<u64>, DeliveryError> {
        let until = match (self.pending, join_until) {
            (Some(p), Some(until)) => p.until.min(until),
            (Some(p), None) => p.until,
            (None, Some(until)) => until,
            (None, None) => return Ok(None),
        };
        if now < self.next_allowed || self.pending.is_some_and(|p| now < p.ready) {
            return Ok(None);
        }
        self.next_allowed = match deadline(now, self.interval) {
            Ok(until) => until,
            Err(error) => {
                self.close();
                return Err(error);
            }
        };
        self.pending = None;
        Ok(Some(until))
    }
    pub const fn next_deadline(&self) -> Option<u64> {
        match self.pending {
            Some(p) if !self.closed => Some(if p.ready < p.until { p.ready } else { p.until }),
            _ => None,
        }
    }
    /// Abandon queued work, without replenishing the encoder's rate allowance.
    pub fn cancel_pending(&mut self) {
        self.pending = None;
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.pending = None;
    }
    fn check_clock(&mut self, now: u64) -> Result<(), DeliveryError> {
        if self.closed {
            return Err(DeliveryError::WrongState);
        }
        if self.last_now.is_some_and(|last| now < last) {
            self.close();
            return Err(DeliveryError::ClockRegression);
        }
        self.last_now = Some(now);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delivery::{MediaBindings, MediaEpoch, RecoveryDisposition, SendPolicy};
    use fr_core::{ids::*, limits::ProtocolLimits};
    use fr_wire::{
        MediaLimits,
        decoder::Binding,
        input::{InputDelivery, InputDirection},
        negotiation::ControlBinding,
        recovery_request::{self, Reason, Request},
    };

    fn sender(horizon: u64) -> SendCache {
        SendCache::new(
            MediaLimits::new(ProtocolLimits::ABSOLUTE, 1150, 16_384, 64).unwrap(),
            MediaBindings::new(1, 2, 3, 4).unwrap(),
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL,
            },
            SendPolicy {
                recovery_horizon_micros: horizon,
                ..SendPolicy::default()
            },
        )
        .unwrap()
    }
    fn demand(cache: &mut SendCache, now: u64) -> RecoveryDemand {
        let binding = Binding {
            parent: ControlBinding {
                id: 10,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            display: 4,
            geometry: DisplayGeometryGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
        };
        let mut bytes = [0; recovery_request::REQUEST_BYTES];
        recovery_request::encode(
            Request {
                reason: Reason::ReferenceExpired,
                last_useful_frame: None,
            },
            binding,
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        match cache.request_recovery(&bytes, binding, now).unwrap() {
            RecoveryDisposition::Accepted(demand) => demand,
            RecoveryDisposition::Coalesced => panic!("expected a new request"),
        }
    }
    fn queue(idr: &mut IdrCoalescer, cache: &mut SendCache, now: u64) {
        let demand = demand(cache, now);
        idr.queue(cache, demand, now).unwrap();
    }
    #[test]
    fn a_late_viewer_does_not_inherit_an_expired_cohorts_deadline() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        let mut old = sender(100);
        let mut new = sender(2_000_000);
        queue(&mut idr, &mut old, 0);
        // The capture owner has not polled take() yet. Expired work belongs to
        // the failed cohort, not the next viewer admitted on this same source.
        queue(&mut idr, &mut new, 101);
        assert_eq!(idr.next_deadline(), Some(101));
        assert_eq!(idr.take(101).unwrap(), Some(2_000_101));
        assert_eq!(
            old.tick(101),
            Err(crate::delivery::SendError::Delivery(
                DeliveryError::RecoveryExpired
            ))
        );
        assert_eq!(new.next_deadline(), Some(2_000_101));
    }
    #[test]
    fn the_exact_expiry_boundary_starts_a_new_cohort() {
        let mut idr = IdrCoalescer::new(100_000).unwrap();
        queue(&mut idr, &mut sender(100), 0);
        queue(&mut idr, &mut sender(2_000_000), 100);
        assert_eq!(idr.take(100).unwrap(), Some(2_000_100));
    }
    #[test]
    fn an_unexpired_cohort_keeps_its_original_deadline() {
        let mut idr = IdrCoalescer::new(100_000).unwrap();
        queue(&mut idr, &mut sender(100), 0);
        queue(&mut idr, &mut sender(2_000_000), 99);
        assert_eq!(idr.take(99).unwrap(), Some(100));
    }
    #[test]
    fn retiring_expired_work_never_refills_encoder_rate_credit() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(2_000_000), 0);
        assert_eq!(idr.take(0).unwrap(), Some(2_000_000));
        queue(&mut idr, &mut sender(100_000), 1);
        // Even repeated expiry/arrival cycles cannot create an early enqueue.
        for now in [100_001, 200_001, 300_001, 400_001] {
            queue(&mut idr, &mut sender(100_000), now);
            assert_eq!(idr.take(now).unwrap(), None);
        }
        queue(&mut idr, &mut sender(2_000_000), 500_001);
        assert_eq!(idr.take(500_001).unwrap(), Some(2_500_001));
        queue(&mut idr, &mut sender(2_000_000), 500_002);
        assert_eq!(idr.next_deadline(), Some(1_000_001));
        assert_eq!(idr.take(1_000_000).unwrap(), None);
        assert_eq!(idr.take(1_000_001).unwrap(), Some(2_500_002));
    }
    #[test]
    fn invalid_demands_cannot_clear_or_extend_another_cohort() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(2_000_000), 0);
        let mut foreign = sender(2_000_000);
        let wrong_owner = sender(2_000_000);
        assert_eq!(
            idr.queue(&wrong_owner, demand(&mut foreign, 1), 1),
            Err(DeliveryError::StaleGeneration)
        );
        let mut expired = sender(10);
        let proof = demand(&mut expired, 2);
        assert_eq!(
            idr.queue(&expired, proof, 12),
            Err(DeliveryError::RecoveryExpired)
        );
        assert_eq!(idr.next_deadline(), Some(0));
        assert_eq!(idr.take(12).unwrap(), Some(2_000_000));
    }
    #[test]
    fn joins_and_recoveries_share_one_rate_allowance_and_earliest_deadline() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        assert_eq!(idr.take_for_join(0, 2_000_000).unwrap(), Some(2_000_000));
        let mut cache = sender(2_000_000);
        queue(&mut idr, &mut cache, 1);
        assert_eq!(idr.take(1).unwrap(), None);
        assert_eq!(idr.take_for_join(499_999, 900_000).unwrap(), None);
        assert_eq!(idr.take_for_join(500_000, 900_000).unwrap(), Some(900_000));
        assert_eq!(idr.next_deadline(), None);
        assert_eq!(idr.take(500_000).unwrap(), None);
        assert_eq!(idr.take_for_join(999_999, 2_000_000).unwrap(), None);
        assert_eq!(
            idr.take_for_join(1_000_000, 2_000_000).unwrap(),
            Some(2_000_000)
        );
    }
    #[test]
    fn join_expiry_and_cancellation_do_not_refill_or_extend_recovery_credit() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(100), 0);
        assert_eq!(idr.take_for_join(0, 0), Err(DeliveryError::RecoveryExpired));
        assert_eq!(idr.next_deadline(), Some(0));
        assert_eq!(idr.take_for_join(1, 2_000_000).unwrap(), Some(100));
        idr.cancel_pending();
        assert_eq!(idr.take_for_join(500_000, 2_000_000).unwrap(), None);
        assert_eq!(
            idr.take_for_join(500_001, 2_000_000).unwrap(),
            Some(2_000_000)
        );
    }
    #[test]
    fn fresh_join_retires_expired_demand_but_never_revives_closed_scheduler() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(100), 0);
        assert_eq!(idr.take_for_join(100, 2_000_000).unwrap(), Some(2_000_000));
        idr.close();
        assert_eq!(
            idr.take_for_join(1_000_000, 2_000_000),
            Err(DeliveryError::WrongState)
        );
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        idr.take_for_join(100, 2_000_000).unwrap();
        assert_eq!(
            idr.take_for_join(99, 2_000_000),
            Err(DeliveryError::ClockRegression)
        );
    }
}
