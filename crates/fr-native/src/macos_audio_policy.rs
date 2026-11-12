//! Portable checks used at the actual Core Audio output boundary.
//!
//! Native callbacks only return allocation credit. They never advance the
//! playback clock or prove that queued sound was played. All positions below
//! are readings of the original Audio Queue's 48 kHz timeline.

pub const STARTUP_US: u64 = 2_000_000;
pub const MAX_QUEUE_SAMPLES: u64 = 40 * 48;
pub const CLOCK_STALL_US: u64 = 50_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Selection,
    Configuration,
    Allocation,
    Unavailable,
    Native,
    Pending,
    Closed,
    Denied,
    Expired,
    Clock,
    BufferLimit,
    DeviceChanged,
    Suspended,
    Backpressure,
    Metadata,
    SubmissionUnknown,
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "macos-audio: {self:?}")
    }
}
impl std::error::Error for Error {}

/// An observed native timeline, with its independent local monotonic sample.
/// No elapsed-time extrapolation or synthetic startup sample position exists.
#[derive(Debug)]
pub struct Clock {
    opened_at: u64,
    last_now: u64,
    last_samples: Option<u64>,
    progressed_at: u64,
    qualified: bool,
}
impl Clock {
    pub const fn new(now: u64) -> Self {
        Self {
            opened_at: now,
            last_now: now,
            last_samples: None,
            progressed_at: now,
            qualified: false,
        }
    }

    /// Observe one real native read. Readiness requires actual forward motion,
    /// not just successful allocation or an `AudioQueueStart` return code.
    pub fn observe(&mut self, now: u64, samples: u64) -> Result<bool, Error> {
        if now < self.last_now || self.last_samples.is_some_and(|last| samples < last) {
            return Err(Error::Clock);
        }
        if !self.qualified && now.saturating_sub(self.opened_at) >= STARTUP_US {
            return Err(Error::Expired);
        }
        if let Some(last) = self.last_samples {
            let elapsed = now - self.last_now;
            // A discontinuity must not be laundered into a fresh output epoch.
            // The native timeline also reports discontinuities independently.
            let largest_step = elapsed
                .checked_mul(48)
                .map(|n| n.div_ceil(1000))
                .and_then(|n| n.checked_add(MAX_QUEUE_SAMPLES))
                .ok_or(Error::Clock)?;
            if samples - last > largest_step {
                return Err(Error::Clock);
            }
            if samples > last {
                self.qualified = true;
                self.progressed_at = now;
            } else if self.qualified && now - self.progressed_at >= CLOCK_STALL_US {
                return Err(Error::Clock);
            }
        } else {
            self.progressed_at = now;
        }
        self.last_now = now;
        self.last_samples = Some(samples);
        Ok(self.qualified)
    }

    pub fn pending(&mut self, now: u64) -> Result<(), Error> {
        if now < self.last_now {
            return Err(Error::Clock);
        }
        if self.qualified {
            return Err(Error::Suspended);
        }
        if now.saturating_sub(self.opened_at) >= STARTUP_US {
            return Err(Error::Expired);
        }
        self.last_now = now;
        Ok(())
    }
}

/// Immutable bounds of one decoded frame. `slot_end` is independent of its
/// wall-clock expiry: neither bound can be replaced after native preparation.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    pub slot_start: u64,
    pub slot_end: u64,
    pub valid_until: u64,
    pub samples: u64,
    pub lead: u64,
}
impl Window {
    pub fn scheduled(self) -> Result<(u64, u64), Error> {
        if self.samples == 0 || self.slot_start.checked_add(self.samples) != Some(self.slot_end) {
            return Err(Error::Metadata);
        }
        let start = self.slot_start.checked_add(self.lead).ok_or(Error::Clock)?;
        let end = start.checked_add(self.samples).ok_or(Error::Clock)?;
        // AudioTimeStamp uses a Float64: retain exact integer sample positions.
        if end > (1_u64 << 53) {
            return Err(Error::Clock);
        }
        Ok((start, end))
    }

    /// Called again by the native write's final permission callback AFTER its
    /// buffer copy and real timeline query. Includes the entire queued extent,
    /// even if Audio Queue already returned its application buffer for reuse.
    pub fn validate(self, now: u64, consumed: u64) -> Result<(u64, u64), Error> {
        let (start, end) = self.scheduled()?;
        if now >= self.valid_until {
            return Err(Error::Expired);
        }
        if consumed < self.slot_start || consumed >= self.slot_end || consumed > start {
            return Err(Error::Clock);
        }
        let remaining = end.checked_sub(consumed).ok_or(Error::Clock)?;
        if remaining > MAX_QUEUE_SAMPLES {
            return Err(Error::BufferLimit);
        }
        let remaining_us = remaining
            .checked_mul(1000)
            .ok_or(Error::Clock)?
            .div_ceil(48);
        if now
            .checked_add(remaining_us)
            .is_none_or(|completion| completion >= self.valid_until)
        {
            return Err(Error::Expired);
        }
        Ok((start, end))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window() -> Window {
        Window {
            slot_start: 1000,
            slot_end: 1480,
            valid_until: 100_000,
            samples: 480,
            lead: 480,
        }
    }

    #[test]
    fn readiness_requires_native_clock_progress_not_elapsed_wall_time() {
        let mut clock = Clock::new(100);
        assert!(!clock.observe(200, 0).unwrap());
        assert!(!clock.observe(20_000, 0).unwrap());
        assert!(clock.observe(21_000, 48).unwrap());
        assert!(clock.observe(22_000, 96).unwrap());
    }

    #[test]
    fn silent_startup_keeps_one_original_deadline() {
        let mut clock = Clock::new(100);
        clock.pending(1_000_000).unwrap();
        assert!(!clock.observe(2_000_099, 0).unwrap());
        assert_eq!(clock.observe(2_000_100, 1), Err(Error::Expired));
    }

    #[test]
    fn device_stall_and_discontinuity_cannot_relabel_the_clock() {
        let mut clock = Clock::new(0);
        assert!(!clock.observe(0, 10).unwrap());
        assert!(clock.observe(1000, 58).unwrap());
        assert_eq!(clock.observe(51_000, 58), Err(Error::Clock));
        let mut clock = Clock::new(0);
        clock.observe(0, 10).unwrap();
        assert_eq!(clock.observe(1000, 100_000), Err(Error::Clock));
    }

    #[test]
    fn both_clock_regressions_and_loss_of_a_ready_device_refuse() {
        let mut clock = Clock::new(10);
        assert_eq!(clock.observe(9, 0), Err(Error::Clock));
        clock.observe(10, 100).unwrap();
        assert_eq!(clock.observe(11, 99), Err(Error::Clock));
        clock.observe(1010, 148).unwrap();
        assert_eq!(clock.pending(2010), Err(Error::Suspended));
    }

    #[test]
    fn native_preparation_cannot_extend_either_original_deadline() {
        let frame = window();
        assert_eq!(frame.validate(10_000, 1000), Ok((1480, 1960)));
        assert_eq!(frame.validate(100_000, 1000), Err(Error::Expired));
        assert_eq!(frame.validate(10_000, 1480), Err(Error::Clock));
        assert_eq!(frame.validate(10_000, 999), Err(Error::Clock));
    }

    #[test]
    fn returned_buffers_do_not_refund_native_queued_sample_extent() {
        let mut frame = window();
        frame.lead = MAX_QUEUE_SAMPLES - frame.samples;
        assert!(frame.validate(10_000, 1000).is_ok());
        frame.lead += 1;
        assert_eq!(frame.validate(10_000, 1000), Err(Error::BufferLimit));
    }

    #[test]
    fn sound_must_finish_before_the_original_wall_deadline() {
        let mut frame = window();
        frame.valid_until = 30_000;
        assert_eq!(frame.validate(10_000, 1000), Err(Error::Expired));
        frame.valid_until += 1;
        assert!(frame.validate(10_000, 1000).is_ok());
    }

    #[test]
    fn malformed_shapes_and_imprecise_native_positions_are_refused() {
        let mut frame = window();
        frame.samples += 1;
        assert_eq!(frame.scheduled(), Err(Error::Metadata));
        let frame = Window {
            slot_start: (1_u64 << 53) - 1,
            slot_end: (1_u64 << 53) + 1,
            samples: 2,
            ..window()
        };
        assert_eq!(frame.scheduled(), Err(Error::Clock));
        let frame = Window {
            lead: u64::MAX,
            ..window()
        };
        assert_eq!(frame.scheduled(), Err(Error::Clock));
    }
}
