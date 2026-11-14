//! Original native menu choices. An old queued command cannot acquire the
//! choice displayed by a later popup or be retried with a new timestamp.
#![forbid(unsafe_code)]

use crate::keys::{TICK_MARGIN_MS, message_time};
use std::time::Duration;

pub struct Choice<T> {
    original: Option<T>,
    opened: u64,
}

impl<T> Choice<T> {
    pub const fn new(original: T, opened: u64) -> Self {
        Self {
            original: Some(original),
            opened,
        }
    }

    pub fn take(&mut self, message: u32, now: u64) -> Option<(T, Duration)> {
        let original = self.original.take()?;
        let at = message_time(message, now).ok()?;
        // The ambiguous opening tick is not positive evidence of a choice
        // made from this popup. A new menu visit is needed after refusal.
        if at <= self.opened {
            return None;
        }
        Some((original, Duration::from_millis(now - at + TICK_MARGIN_MS)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_menu_choice_is_consumed_once_with_its_queue_age() {
        let mut choice = Choice::new(19, 1_000);
        assert_eq!(
            choice.take(1_020, 1_040),
            Some((19, Duration::from_millis(36)))
        );
        assert!(choice.take(1_050, 1_050).is_none());
    }

    #[test]
    fn old_same_tick_future_and_expired_commands_consume_no_new_choice() {
        for (message, now) in [(999, 1_010), (1_000, 1_010), (1_020, 1_019), (1_010, 1_094)] {
            let mut choice = Choice::new(23, 1_000);
            assert!(choice.take(message, now).is_none());
            assert!(choice.take(1_100, 1_100).is_none());
        }
    }

    #[test]
    fn wrapped_native_timestamps_keep_the_original_popup_boundary() {
        let wrap = 1_u64 << 32;
        let mut choice = Choice::new(29, wrap - 10);
        assert_eq!(
            choice.take(5, wrap + 6),
            Some((29, Duration::from_millis(17)))
        );
        let mut replacement = Choice::new(31, wrap + 10);
        assert!(replacement.take(5, wrap + 20).is_none());
    }
}
