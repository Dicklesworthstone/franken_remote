//! One explicit native file choice: its original dialog budget and accepted
//! gesture age survive native cleanup, descriptor opening and UI delivery.
#![forbid(unsafe_code)]

pub const CHOICE_MS: u64 = 120_000;
pub const ACCEPTED_MS: u64 = 2_000;
pub const MAX_PATH_UNITS: usize = 32_768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Cancelled,
    Expired,
    Clock,
    InvalidPath,
    Native,
    Selection,
    Cleanup,
    Used,
}

pub struct Choice {
    until: u64,
    last: u64,
    shown: Option<u64>,
    accepted: Option<u64>,
    refused: Option<Error>,
    consumed: bool,
}

impl Choice {
    pub fn new(now: u64) -> Result<Self, Error> {
        Ok(Self {
            until: now.checked_add(CHOICE_MS).ok_or(Error::Clock)?,
            last: now,
            shown: None,
            accepted: None,
            refused: None,
            consumed: false,
        })
    }

    pub fn check(&mut self, now: u64) -> Result<(), Error> {
        if self.consumed {
            return Err(Error::Used);
        }
        if let Some(error) = self.refused {
            return Err(error);
        }
        let failure = if now < self.last {
            Some(Error::Clock)
        } else if now >= self.until
            || self
                .accepted
                .is_some_and(|at| now.saturating_sub(at) >= ACCEPTED_MS)
        {
            Some(Error::Expired)
        } else {
            None
        };
        self.last = now;
        if let Some(error) = failure {
            return self.refuse(error);
        }
        Ok(())
    }

    pub fn accept(&mut self, message: u32, now: u64) -> Result<(), Error> {
        self.check(now)?;
        if self.accepted.is_some() {
            return self.refuse(Error::Used);
        }
        let Ok(at) = crate::keys::message_time(message, now) else {
            return self.refuse(Error::Expired);
        };
        if self.shown.is_none_or(|shown| at <= shown) {
            return self.refuse(Error::Expired);
        }
        // Charge native timestamp rounding too, before any filesystem work.
        let Some(at) = at.checked_sub(crate::keys::TICK_MARGIN_MS) else {
            return self.refuse(Error::Clock);
        };
        self.accepted = Some(at);
        self.check(now)
    }

    pub fn shown(&mut self, now: u64) -> Result<(), Error> {
        self.check(now)?;
        if self.shown.replace(now).is_some() {
            return self.refuse(Error::Used);
        }
        Ok(())
    }

    pub fn refuse(&mut self, error: Error) -> Result<(), Error> {
        if self.consumed {
            return Err(Error::Used);
        }
        Err(*self.refused.get_or_insert(error))
    }

    pub fn finish(&mut self, now: u64) -> Result<(), Error> {
        self.check(now)?;
        if self.accepted.is_none() {
            return self.refuse(Error::InvalidPath);
        }
        self.consumed = true;
        Ok(())
    }

    pub fn selected(&self) -> bool {
        self.accepted.is_some() && self.refused.is_none() && !self.consumed
    }
}

/// A single terminated path only. Reject invalid UTF-16 and a second selected
/// name; no lossily converted, truncated, or embedded-NUL selection is opened.
pub fn path_units(units: &[u16]) -> Result<&[u16], Error> {
    if units.len() > MAX_PATH_UNITS {
        return Err(Error::InvalidPath);
    }
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .ok_or(Error::InvalidPath)?;
    if end == 0
        || units[end + 1..].iter().any(|unit| *unit != 0)
        || char::decode_utf16(units[..end].iter().copied()).any(|value| value.is_err())
    {
        return Err(Error::InvalidPath);
    }
    Ok(&units[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shown(now: u64) -> Choice {
        let mut choice = Choice::new(now).unwrap();
        choice.shown(now).unwrap();
        choice
    }

    #[test]
    fn commands_from_before_the_original_dialog_was_shown_cannot_accept_a_path() {
        for message in [1000, 1001] {
            let mut choice = Choice::new(900).unwrap();
            choice.shown(1001).unwrap();
            assert_eq!(choice.accept(message, 1002), Err(Error::Expired));
            assert_eq!(choice.finish(1003), Err(Error::Expired));
        }
    }

    #[test]
    fn the_original_two_minute_choice_budget_never_slides() {
        let mut choice = shown(1000);
        choice.check(120_999).unwrap();
        assert_eq!(choice.accept(121_000, 121_000), Err(Error::Expired));
        assert_eq!(choice.finish(121_000), Err(Error::Expired));
    }

    #[test]
    fn cleanup_and_handle_preparation_spend_the_original_accepted_gesture_age() {
        let mut choice = shown(1000);
        choice.accept(2000, 2000).unwrap();
        choice.check(3983).unwrap();
        assert_eq!(choice.finish(3984), Err(Error::Expired));
    }

    #[test]
    fn cancelled_or_consumed_choices_never_publish_a_second_file() {
        let mut cancelled = shown(1000);
        cancelled.accept(2000, 2000).unwrap();
        assert_eq!(cancelled.refuse(Error::Cancelled), Err(Error::Cancelled));
        assert_eq!(cancelled.finish(2001), Err(Error::Cancelled));
        let mut selected = shown(1000);
        selected.accept(2000, 2000).unwrap();
        selected.finish(2001).unwrap();
        assert_eq!(selected.finish(2002), Err(Error::Used));
    }

    #[test]
    fn native_clock_regression_stale_clicks_and_counter_overflow_refuse() {
        assert!(matches!(Choice::new(u64::MAX), Err(Error::Clock)));
        let mut choice = shown(1000);
        assert_eq!(choice.check(999), Err(Error::Clock));
        let mut stale = shown(1000);
        assert_eq!(stale.accept(2000, 2084), Err(Error::Expired));
        let mut future = shown(1000);
        assert_eq!(future.accept(2001, 2000), Err(Error::Expired));
    }

    #[test]
    fn native_message_wrap_is_charged_without_restamping() {
        let now = u64::from(u32::MAX) + 11;
        let mut choice = shown(now - 1000);
        choice.accept(u32::MAX - 9, now).unwrap();
        assert_eq!(choice.finish(now + ACCEPTED_MS - 36), Err(Error::Expired));
    }

    #[test]
    fn paths_are_single_bounded_complete_unicode_values() {
        let valid: Vec<_> = "C:\\chosen\\résumé.txt"
            .encode_utf16()
            .chain([0, 0])
            .collect();
        assert_eq!(path_units(&valid).unwrap().len(), valid.len() - 2);
        for invalid in [
            vec![],
            vec![0],
            vec![65],
            vec![65, 0, 66, 0],
            vec![0xd800, 0],
        ] {
            assert_eq!(path_units(&invalid), Err(Error::InvalidPath));
        }
        assert_eq!(
            path_units(&vec![0; MAX_PATH_UNITS + 1]),
            Err(Error::InvalidPath)
        );
    }
}
