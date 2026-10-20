//! Fixed-space shared-encoder IDR admission; never a global pipeline reset.
use super::{DeliveryError, RecoveryDemand, SendCache, deadline};

#[derive(Debug, Clone, Copy)]
struct Pending {
    ready: u64,
    // Private capture retains its current cohort's earliest deadline.
    until: u64,
    // Shared-source scheduling retains the latest ORIGINAL accepted deadline.
    // This is not permission to deliver to a consumer past its own deadline.
    shared_until: u64,
}
/// Keep one coalescer for the shared encoder lifetime, not one per viewer or
/// recovery generation. Accepted requests occupy ONE slot. Private admission
/// retains the first cohort's deadline; shared-source admission can serve a later
/// live request independently. Neither changes a sender's recovery deadline.
/// New generations and cancellation do not refill rate credit.
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
        // Consume the unique proof even when another viewer already queued work.
        drop(demand);
        match &mut self.pending {
            Some(p) => {
                // Preserve private admission's existing cohort replacement when
                // its earliest request expired before the owner polled take().
                // A different already accepted request may still justify shared
                // source work; replacing that private cohort must not erase it.
                if now >= p.until {
                    p.ready = now.max(self.next_allowed);
                    p.until = until;
                } else {
                    p.until = p.until.min(until);
                }
                p.shared_until = p.shared_until.max(until);
            }
            None => {
                self.pending = Some(Pending {
                    ready: now.max(self.next_allowed),
                    until,
                    shared_until: until,
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
    /// Admit an IDR for an independently owned shared source while at least one
    /// accepted request's original deadline remains unexpired. An earlier failed
    /// consumer cannot discard a later live request or cap the shared worker's
    /// lifetime. All requests still consume the SAME encoder rate allowance.
    ///
    /// The returned latest deadline is scheduling evidence only, never a new
    /// consumer deadline or observation/delivery authority. The source needs its
    /// own bounded work deadline and live observation checks; each sender must
    /// still refuse replacement and delivery after its original recovery expiry.
    /// Pair this with `next_shared_deadline`, not the private cohort's deadline.
    pub fn take_shared(&mut self, now: u64) -> Result<Option<u64>, DeliveryError> {
        self.check_clock(now)?;
        let Some(pending) = self.pending else {
            return Ok(None);
        };
        if now >= pending.shared_until {
            self.pending = None;
            return Err(DeliveryError::RecoveryExpired);
        }
        self.admit_until(now, pending.shared_until)
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
        self.admit_until(now, until)
    }
    fn admit_until(&mut self, now: u64, until: u64) -> Result<Option<u64>, DeliveryError> {
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
    /// Next shared-source rate admission or expiry of ALL accepted requests.
    /// An expired earliest consumer does not create a past-due wake while a
    /// later request is still live but waiting for the unchanged rate allowance.
    pub const fn next_shared_deadline(&self) -> Option<u64> {
        match self.pending {
            Some(p) if !self.closed => Some(if p.ready < p.shared_until {
                p.ready
            } else {
                p.shared_until
            }),
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
    use crate::delivery::{MediaBindings, MediaEpoch, RecoveryDisposition, SendError, SendPolicy};
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
    fn replace(cache: &mut SendCache, now: u64) -> Result<(), SendError> {
        cache.replace(
            MediaEpoch {
                configuration: CodecConfigurationGeneration::INITIAL,
                recovery: RecoveryGeneration::INITIAL.next().unwrap(),
            },
            MediaBindings::new(5, 6, 7, 8).unwrap(),
            now,
        )
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
    #[test]
    fn shared_admission_preserves_later_live_demand_and_original_sender_deadlines() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        let mut early = sender(100);
        let mut later = sender(2_000_000);
        queue(&mut idr, &mut early, 0);
        queue(&mut idr, &mut later, 99);
        // Both demands were accepted before the first expired. The second
        // sender cannot issue a replacement proof merely because time passed.
        assert_eq!(idr.take_shared(100).unwrap(), Some(2_000_099));
        assert_eq!(early.next_deadline(), Some(100));
        assert_eq!(later.next_deadline(), Some(2_000_099));
        assert_eq!(
            replace(&mut early, 100),
            Err(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
        assert_eq!(later.tick(100), Err(SendError::NeedsRecovery));
        assert_eq!(
            replace(&mut later, 2_000_099),
            Err(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
        assert_eq!(idr.next_shared_deadline(), None);
        assert_eq!(idr.take_shared(2_000_099).unwrap(), None);
    }
    #[test]
    fn private_admission_keeps_earliest_expiry_despite_later_shared_demand() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        let mut early = sender(100);
        let mut later = sender(2_000_000);
        queue(&mut idr, &mut early, 0);
        queue(&mut idr, &mut later, 99);
        assert_eq!(idr.take(100), Err(DeliveryError::RecoveryExpired));
        assert_eq!(idr.next_deadline(), None);
        assert_eq!(idr.next_shared_deadline(), None);
        assert_eq!(idr.take_shared(100).unwrap(), None);
        assert_eq!(later.next_deadline(), Some(2_000_099));
    }
    #[test]
    fn shared_rate_wait_does_not_wake_at_an_expired_earliest_deadline() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        assert_eq!(idr.take_for_join(0, 2_000_000).unwrap(), Some(2_000_000));
        let mut early = sender(100);
        let mut later = sender(2_000_000);
        queue(&mut idr, &mut early, 1);
        queue(&mut idr, &mut later, 2);
        assert_eq!(idr.next_deadline(), Some(101));
        assert_eq!(idr.next_shared_deadline(), Some(500_000));
        for now in [101, 200_000, 499_999] {
            assert_eq!(idr.take_shared(now).unwrap(), None);
            assert_eq!(idr.next_shared_deadline(), Some(500_000));
        }
        assert_eq!(idr.take_shared(500_000).unwrap(), Some(2_000_002));
        assert_eq!(later.next_deadline(), Some(2_000_002));
        assert_eq!(
            replace(&mut early, 500_000),
            Err(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
        assert_eq!(replace(&mut later, 500_000), Ok(()));
    }
    #[test]
    fn replacing_private_cohort_retains_previously_accepted_shared_demand() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        idr.take_for_join(0, 2_000_000).unwrap();
        let mut early = sender(100);
        let mut later = sender(2_000_000);
        let mut newest = sender(100);
        queue(&mut idr, &mut early, 1);
        queue(&mut idr, &mut later, 2);
        queue(&mut idr, &mut newest, 101);
        // The new private cohort has its own unchanged earliest deadline. It
        // cannot erase the longer request already accepted by this same source.
        assert_eq!(idr.next_deadline(), Some(201));
        assert_eq!(idr.next_shared_deadline(), Some(500_000));
        assert_eq!(idr.take_shared(201).unwrap(), None);
        assert_eq!(idr.take_shared(500_000).unwrap(), Some(2_000_002));
        assert_eq!(later.next_deadline(), Some(2_000_002));
        assert_eq!(
            replace(&mut newest, 500_000),
            Err(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
        assert_eq!(replace(&mut later, 500_000), Ok(()));
    }
    #[test]
    fn wholly_expired_shared_demand_is_retired_without_refilling_credit() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        idr.take_for_join(0, 2_000_000).unwrap();
        let mut early = sender(10);
        let mut later = sender(20);
        queue(&mut idr, &mut early, 1);
        queue(&mut idr, &mut later, 2);
        assert_eq!(idr.next_shared_deadline(), Some(22));
        assert_eq!(idr.take_shared(21).unwrap(), None);
        assert_eq!(idr.take_shared(22), Err(DeliveryError::RecoveryExpired));
        assert_eq!(idr.next_deadline(), None);
        assert_eq!(idr.next_shared_deadline(), None);
        for cache in [&mut early, &mut later] {
            assert_eq!(
                replace(cache, 22),
                Err(SendError::Delivery(DeliveryError::RecoveryExpired))
            );
        }
        let mut fresh = sender(2_000_000);
        queue(&mut idr, &mut fresh, 100);
        assert_eq!(idr.take_shared(499_999).unwrap(), None);
        assert_eq!(idr.take_shared(500_000).unwrap(), Some(2_000_100));
    }
    #[test]
    fn shared_private_and_join_admission_charge_the_same_encoder_allowance() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(2_000_000), 0);
        assert_eq!(idr.take_shared(0).unwrap(), Some(2_000_000));
        assert_eq!(idr.take_for_join(499_999, 2_000_000).unwrap(), None);
        assert_eq!(
            idr.take_for_join(500_000, 2_000_000).unwrap(),
            Some(2_000_000)
        );
        queue(&mut idr, &mut sender(2_000_000), 500_001);
        assert_eq!(idr.take(999_999).unwrap(), None);
        assert_eq!(idr.take(1_000_000).unwrap(), Some(2_500_001));
        queue(&mut idr, &mut sender(2_000_000), 1_000_001);
        assert_eq!(idr.take_shared(1_499_999).unwrap(), None);
        assert_eq!(idr.take_shared(1_500_000).unwrap(), Some(3_000_001));
    }
    #[test]
    fn cancelling_shared_demand_does_not_refund_rate_or_change_sender_expiry() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(2_000_000), 0);
        idr.take_shared(0).unwrap();
        let mut early = sender(100);
        let mut later = sender(2_000_000);
        queue(&mut idr, &mut early, 1);
        queue(&mut idr, &mut later, 2);
        idr.cancel_pending();
        assert_eq!(idr.next_deadline(), None);
        assert_eq!(idr.next_shared_deadline(), None);
        assert_eq!(idr.take_shared(101).unwrap(), None);
        assert_eq!(later.next_deadline(), Some(2_000_002));
        assert_eq!(
            replace(&mut early, 101),
            Err(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
        queue(&mut idr, &mut sender(2_000_000), 101);
        assert_eq!(idr.take_shared(499_999).unwrap(), None);
        assert_eq!(idr.take_shared(500_000).unwrap(), Some(2_000_101));
    }
    #[test]
    fn foreign_closed_and_replaced_demands_cannot_extend_shared_scheduling() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(100), 0);
        let mut foreign = sender(2_000_000);
        let mut unrelated = sender(2_000_000);
        assert_eq!(
            idr.queue(&unrelated, demand(&mut foreign, 1), 1),
            Err(DeliveryError::StaleGeneration)
        );
        let closed_proof = demand(&mut unrelated, 2);
        unrelated.close();
        assert_eq!(
            idr.queue(&unrelated, closed_proof, 2),
            Err(DeliveryError::StaleGeneration)
        );
        let mut replaced = sender(2_000_000);
        let old_proof = demand(&mut replaced, 3);
        replace(&mut replaced, 4).unwrap();
        assert_eq!(
            idr.queue(&replaced, old_proof, 4),
            Err(DeliveryError::StaleGeneration)
        );
        let mut expired = sender(1);
        let expired_proof = demand(&mut expired, 5);
        assert_eq!(
            idr.queue(&expired, expired_proof, 6),
            Err(DeliveryError::RecoveryExpired)
        );
        assert_eq!(idr.take_shared(100), Err(DeliveryError::RecoveryExpired));
        assert_eq!(idr.next_shared_deadline(), None);
    }
    #[test]
    fn dropped_unqueued_demand_and_reused_sender_numbers_cannot_extend_shared_work() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(100), 0);
        let mut unqueued = sender(2_000_000);
        drop(demand(&mut unqueued, 1));
        let mut original = sender(2_000_000);
        let orphan = demand(&mut original, 2);
        drop(original);
        let mut reused_numbers = sender(2_000_000);
        // Even a sender with the same generations, request and exact deadline
        // owns a different cache. Dropping either proof never adds source work.
        drop(demand(&mut reused_numbers, 2));
        assert_eq!(
            idr.queue(&reused_numbers, orphan, 2),
            Err(DeliveryError::StaleGeneration)
        );
        assert_eq!(idr.take_shared(100), Err(DeliveryError::RecoveryExpired));
        assert_eq!(idr.take_shared(2_000_002).unwrap(), None);
        assert_eq!(
            replace(&mut unqueued, 2_000_002),
            Err(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
    }
    #[test]
    fn shared_clock_regression_and_explicit_close_are_terminal() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(100), 10);
        queue(&mut idr, &mut sender(2_000_000), 20);
        assert_eq!(idr.take_shared(19), Err(DeliveryError::ClockRegression));
        assert_eq!(idr.next_deadline(), None);
        assert_eq!(idr.next_shared_deadline(), None);
        assert_eq!(idr.take_shared(20), Err(DeliveryError::WrongState));
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        queue(&mut idr, &mut sender(2_000_000), 0);
        idr.close();
        assert_eq!(idr.next_shared_deadline(), None);
        assert_eq!(idr.take_shared(0), Err(DeliveryError::WrongState));
    }
    #[test]
    fn shared_rate_deadline_overflow_closes_without_saturating_credit() {
        let mut idr = IdrCoalescer::new(500_000).unwrap();
        let mut cache = sender(1);
        queue(&mut idr, &mut cache, u64::MAX - 2);
        assert_eq!(
            idr.take_shared(u64::MAX - 2),
            Err(DeliveryError::ClockOverflow)
        );
        assert_eq!(idr.next_shared_deadline(), None);
        assert_eq!(
            idr.take_shared(u64::MAX - 1),
            Err(DeliveryError::WrongState)
        );
        assert_eq!(
            replace(&mut cache, u64::MAX - 1),
            Err(SendError::Delivery(DeliveryError::RecoveryExpired))
        );
    }
}
