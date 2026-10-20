//! The canonical viewer's single prepared repair; no transport or runtime.
use fr_media::delivery::{DeliveryError, ReceivePipeline};

// Repair wire grammar admits at most 64 ranges, irrespective of AU size. One
// fixed pending buffer avoids both retained-record growth and allocation churn.
pub(super) struct Repair {
    pub(super) bytes: [u8; 36 + 64 * 8],
    pub(super) len: usize,
    pub(super) until: u64,
    pub(super) frame: u64,
    prepared_at: u64,
}
impl Default for Repair {
    fn default() -> Self {
        Self {
            bytes: [0; 36 + 64 * 8],
            len: 0,
            until: 0,
            frame: 0,
            prepared_at: 0,
        }
    }
}
impl Repair {
    pub(super) fn clear(&mut self) {
        self.bytes.fill(0);
        self.len = 0;
        self.until = 0;
        self.frame = 0;
        self.prepared_at = 0;
    }
    pub(super) fn prepare_raw(
        &mut self,
        receiver: &mut ReceivePipeline,
        current: u64,
    ) -> Result<(), DeliveryError> {
        receiver.tick(current)?;
        if self.len != 0 && !self.useful(receiver, current)? {
            self.clear();
        }
        if self.len == 0
            && let Some(offer) = receiver.repair_offer(current, &mut self.bytes)?
        {
            self.len = offer.bytes;
            self.frame = offer.frame;
            // Repair is useful until the ORIGINAL missing reference expires.
            // An unrelated 80 ms send timeout killed otherwise recoverable
            // desktops while the admitted reference still had time to arrive.
            // Keep this exact offer through backpressure; neither a poll nor
            // transport admission creates a new lifetime or another attempt.
            self.until = offer.reference_deadline_us;
            self.prepared_at = current;
        }
        Ok(())
    }
    /// Read-only pre-admission check. The caller ticks the original receiver
    /// after a refusal; this never restarts a repair or recovery lifetime.
    pub(super) fn useful(
        &self,
        receiver: &ReceivePipeline,
        current: u64,
    ) -> Result<bool, DeliveryError> {
        if current < self.prepared_at {
            return Err(DeliveryError::ClockRegression);
        }
        Ok(self.len != 0 && current < self.until && receiver.repair_needed(self.frame))
    }
}

#[cfg(test)]
#[path = "repair/tests.rs"]
mod tests;
