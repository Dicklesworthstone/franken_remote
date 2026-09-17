//! Correlate logical X timestamps without refreshing old events at dequeue.
use super::{ClientInstant, StopReason};
use frd::session_startup::viewer_events::MAX_EVENT_AGE_US;

pub(super) struct Timeline {
    last_native: u32,
    last_barrier: u32,
    lower: ClientInstant,
}
impl Timeline {
    pub(super) fn new(barrier: u32, before: ClientInstant) -> Self {
        Self {
            last_native: barrier,
            last_barrier: barrier,
            lower: before,
        }
    }
    pub(super) fn barrier(&mut self, time: u32) -> Result<(), StopReason> {
        if time.wrapping_sub(self.last_barrier) >= (1 << 31) {
            return Err(StopReason::Clock);
        }
        self.last_barrier = time;
        Ok(())
    }
    pub(super) fn sample(
        &mut self,
        time: u32,
        barrier: u32,
        before: ClientInstant,
    ) -> Result<ClientInstant, StopReason> {
        if time.wrapping_sub(self.last_native) >= (1 << 31) {
            return Err(StopReason::Clock);
        }
        let age = u64::from(barrier.wrapping_sub(time))
            .checked_add(1)
            .and_then(|n| n.checked_mul(1000))
            .ok_or(StopReason::Clock)?;
        if age >= MAX_EVENT_AGE_US {
            return Err(StopReason::Expired);
        }
        // before <= server barrier time; subtract a full millisecond to include
        // native timestamp quantization. Prior lower bounds are valid for later
        // ordered events too. This is not "now" assigned to a retained event.
        let lower = before.0.checked_sub(age).ok_or(StopReason::Clock)?;
        if before < self.lower {
            return Err(StopReason::Clock);
        }
        self.lower = ClientInstant(lower.max(self.lower.0));
        self.last_native = time;
        Ok(self.lower)
    }
}
