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
        let Some(p) = self.pending else {
            return Ok(None);
        };
        if now >= p.until {
            self.pending = None;
            return Err(DeliveryError::RecoveryExpired);
        }
        if now < p.ready {
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
        Ok(Some(p.until))
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
