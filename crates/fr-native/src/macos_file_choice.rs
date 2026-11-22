//! One bounded local selection before any remote session or control request.
//! Native cleanup and the original choice deadline precede descriptor opening.
#![forbid(unsafe_code)]

use crate::file_drop::{MAX_AGE, paths};
use std::{
    fmt,
    path::PathBuf,
    time::{Duration, Instant},
};

pub(crate) const CHOICE_LIMIT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidSelection,
    Cancelled,
    Expired,
    NativeFailure,
    WrongThread,
    EventFlood,
    AlreadyUsed,
    CleanupIncomplete,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for Error {}

enum State {
    Waiting,
    Selected {
        paths: Vec<PathBuf>,
        captured: Instant,
    },
    Ended(Error),
    Consumed,
}

pub(crate) struct Choice {
    opened: Instant,
    state: State,
    directory_only: bool,
}
impl fmt::Debug for Choice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalFileChoice")
            .field("waiting", &matches!(self.state, State::Waiting))
            .finish_non_exhaustive()
    }
}
impl Choice {
    pub(crate) fn new(opened: Instant) -> Self {
        Self {
            opened,
            state: State::Waiting,
            directory_only: false,
        }
    }
    /// A receiving destination exports exactly one local directory path. The
    /// receiver still opens and verifies that directory after native cleanup.
    pub(crate) fn directory(opened: Instant) -> Self {
        Self {
            directory_only: true,
            ..Self::new(opened)
        }
    }
    pub(crate) fn waiting(&self) -> bool {
        matches!(self.state, State::Waiting)
    }
    pub(crate) fn check(&mut self, now: Instant) -> Result<(), Error> {
        match self.state {
            State::Ended(error) => return Err(error),
            State::Consumed => return Err(Error::AlreadyUsed),
            _ => {}
        }
        let expired = now
            .checked_duration_since(self.opened)
            .is_none_or(|age| age >= CHOICE_LIMIT)
            || matches!(&self.state, State::Selected { captured, .. }
                if now.checked_duration_since(*captured).is_none_or(|age| age >= MAX_AGE));
        if expired {
            return self.refuse(Error::Expired);
        }
        Ok(())
    }
    pub(crate) fn refuse(&mut self, error: Error) -> Result<(), Error> {
        if !matches!(self.state, State::Ended(_) | State::Consumed) {
            self.state = State::Ended(error);
        }
        match self.state {
            State::Ended(original) => Err(original),
            _ => Err(Error::AlreadyUsed),
        }
    }
    pub(crate) fn select(&mut self, bytes: &[u8], age: f64, now: Instant) -> Result<(), Error> {
        self.check(now)?;
        if !self.waiting() {
            return Err(Error::AlreadyUsed);
        }
        if !age.is_finite() || !(0.0..MAX_AGE.as_secs_f64()).contains(&age) {
            return self.refuse(Error::Expired);
        }
        let Some(captured) = now.checked_sub(Duration::from_secs_f64(age)) else {
            return self.refuse(Error::Expired);
        };
        let Ok(selected) = paths(bytes) else {
            return self.refuse(Error::InvalidSelection);
        };
        if self.directory_only
            && (selected.len() != 1
                || selected.iter().any(|path| {
                    path.components()
                        .any(|part| matches!(part, std::path::Component::ParentDir))
                        || path
                            .to_str()
                            .is_none_or(|text| text.chars().any(char::is_control))
                }))
        {
            return self.refuse(Error::InvalidSelection);
        }
        self.state = State::Selected {
            paths: selected,
            captured,
        };
        Ok(())
    }
    /// Native close may fail or take time. Neither outcome can publish a stale
    /// or replacement choice. A successful selection is consumed exactly once.
    pub(crate) fn finish(
        &mut self,
        mut clock: impl FnMut() -> Instant,
        close: impl FnOnce() -> Result<(), Error>,
    ) -> Result<Option<Vec<PathBuf>>, Error> {
        self.check(clock())?;
        if self.waiting() {
            return Ok(None);
        }
        close()?;
        self.check(clock())?;
        match std::mem::replace(&mut self.state, State::Consumed) {
            State::Selected { paths, .. } => Ok(Some(paths)),
            _ => Err(Error::AlreadyUsed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_is_bounded_local_private_and_consumed_after_actual_cleanup() {
        let now = Instant::now();
        let mut choice = Choice::new(now);
        choice
            .select(
                b"file:///private/one\nfile:///private/two%20words\n",
                0.01,
                now,
            )
            .unwrap();
        assert!(!format!("{choice:?}").contains("private"));
        assert_eq!(
            choice.finish(|| now, || Err(Error::CleanupIncomplete)),
            Err(Error::CleanupIncomplete)
        );
        assert_eq!(
            choice.select(b"file:///replacement", 0.0, now),
            Err(Error::AlreadyUsed)
        );
        let selected = choice.finish(|| now, || Ok(())).unwrap().unwrap();
        assert_eq!(
            selected,
            [
                PathBuf::from("/private/one"),
                PathBuf::from("/private/two words")
            ]
        );
        assert_eq!(
            choice.finish(|| now, || panic!("no second native cleanup")),
            Err(Error::AlreadyUsed)
        );
    }

    #[test]
    fn cancellation_or_late_cleanup_cannot_open_selected_sources() {
        let now = Instant::now();
        let mut cancelled = Choice::new(now);
        cancelled.select(b"file:///never-open", 0.0, now).unwrap();
        assert_eq!(cancelled.refuse(Error::Cancelled), Err(Error::Cancelled));
        assert_eq!(
            cancelled.finish(|| now, || panic!("cancelled choice")),
            Err(Error::Cancelled)
        );
        let mut delayed = Choice::new(now);
        delayed.select(b"file:///never-open", 0.0, now).unwrap();
        let mut samples = [now, now + MAX_AGE].into_iter();
        assert_eq!(
            delayed.finish(|| samples.next().unwrap(), || Ok(())),
            Err(Error::Expired)
        );
        assert_eq!(delayed.refuse(Error::Cancelled), Err(Error::Expired));
    }

    #[test]
    fn original_deadline_and_native_completion_age_never_slide() {
        let now = Instant::now();
        let mut expired = Choice::new(now);
        assert_eq!(
            expired.select(b"file:///late", 0.0, now + CHOICE_LIMIT),
            Err(Error::Expired)
        );
        for age in [f64::NAN, f64::INFINITY, -1.0, MAX_AGE.as_secs_f64()] {
            let mut choice = Choice::new(now);
            assert_eq!(
                choice.select(b"file:///late", age, now),
                Err(Error::Expired)
            );
        }
        let mut rollback = Choice::new(now);
        assert_eq!(
            rollback.check(now.checked_sub(Duration::from_millis(1)).unwrap()),
            Err(Error::Expired)
        );
    }

    #[test]
    fn native_payload_cannot_smuggle_remote_duplicate_or_unbounded_paths() {
        let now = Instant::now();
        for bytes in [
            b"".as_slice(),
            b"https://host/file",
            b"file://remote/file",
            b"file:///same\nfile:///same",
            b"file:///bad%00name",
            b"file:///bad%2Fname",
        ] {
            let mut choice = Choice::new(now);
            assert_eq!(choice.select(bytes, 0.0, now), Err(Error::InvalidSelection));
        }
        let oversized = vec![b'x'; crate::file_drop::MAX_BYTES + 1];
        let mut ninth = String::new();
        for index in 0..9 {
            use std::fmt::Write;
            writeln!(ninth, "file:///item{index}").unwrap();
        }
        for bytes in [oversized.as_slice(), ninth.as_bytes()] {
            let mut choice = Choice::new(now);
            assert_eq!(choice.select(bytes, 0.0, now), Err(Error::InvalidSelection));
        }
    }

    #[test]
    fn terminal_native_failure_cannot_be_replaced_by_a_late_completion() {
        let now = Instant::now();
        for error in [Error::NativeFailure, Error::WrongThread, Error::EventFlood] {
            let mut choice = Choice::new(now);
            assert_eq!(choice.refuse(error), Err(error));
            assert_eq!(
                choice.select(b"file:///late-callback", 0.0, now),
                Err(error)
            );
            assert_eq!(
                choice.finish(|| now, || panic!("no selected path")),
                Err(error)
            );
        }
    }

    #[test]
    fn pre_call_projection_counts_native_call_delay_against_original_selection() {
        let before = Instant::now();
        let mut choice = Choice::new(before);
        // Native completed during the poll, then the thread was preempted.
        // The FFI boundary passes its pre-call instant, not a new receipt age.
        choice
            .select(b"file:///stale-completion", 0.0, before)
            .unwrap();
        assert_eq!(
            choice.finish(|| before + MAX_AGE, || panic!("stale native owner")),
            Err(Error::Expired)
        );
    }

    #[test]
    fn directory_mode_preserves_one_exact_path_only_after_cleanup() {
        let now = Instant::now();
        let mut choice = Choice::directory(now);
        choice
            .select(b"file:///private/caf%C3%A9%20%252F\n", 0.0, now)
            .unwrap();
        assert!(!format!("{choice:?}").contains("private"));
        assert_eq!(
            choice.finish(|| now, || Err(Error::CleanupIncomplete)),
            Err(Error::CleanupIncomplete)
        );
        let selected = choice.finish(|| now, || Ok(())).unwrap().unwrap();
        assert_eq!(selected, [PathBuf::from("/private/caf\u{e9} %2F")]);
        assert_eq!(
            choice.finish(|| now, || panic!("original owner already closed")),
            Err(Error::AlreadyUsed)
        );
    }

    #[test]
    fn directory_mode_rejects_multiple_ambiguous_remote_and_control_paths() {
        let now = Instant::now();
        for bytes in [
            b"".as_slice(),
            b"file:///first\nfile:///second",
            b"file://remote/private/folder",
            b"file:///private/../folder",
            b"file:///private/%2e%2e/folder",
            b"file:///private/bad%00folder",
            b"file:///private/bad%C2%85folder",
            b"file:///private/bad%2Ffolder",
            b"file:///private/bad%ff",
        ] {
            let mut choice = Choice::directory(now);
            assert_eq!(choice.select(bytes, 0.0, now), Err(Error::InvalidSelection));
            assert_eq!(
                choice.finish(|| now, || panic!("invalid destination not exported")),
                Err(Error::InvalidSelection)
            );
        }
        let mut choice = Choice::directory(now);
        let oversized = format!("file:///{}", "a".repeat(4096));
        assert_eq!(
            choice.select(oversized.as_bytes(), 0.0, now),
            Err(Error::InvalidSelection)
        );
    }

    #[test]
    fn directory_mode_cancellation_and_native_failure_fence_late_selection() {
        let now = Instant::now();
        for error in [
            Error::Cancelled,
            Error::EventFlood,
            Error::NativeFailure,
            Error::WrongThread,
        ] {
            let mut choice = Choice::directory(now);
            choice.select(b"file:///private/first", 0.0, now).unwrap();
            assert_eq!(choice.refuse(error), Err(error));
            assert_eq!(
                choice.select(b"file:///private/replacement", 0.0, now),
                Err(error)
            );
            assert_eq!(
                choice.finish(|| now, || panic!("refused destination not exported")),
                Err(error)
            );
        }
    }

    #[test]
    fn directory_mode_keeps_original_completion_and_cleanup_deadlines() {
        let now = Instant::now();
        let mut expired = Choice::directory(now);
        assert_eq!(
            expired.select(b"file:///private/late", 0.0, now + CHOICE_LIMIT),
            Err(Error::Expired)
        );
        let mut stale = Choice::directory(now);
        stale.select(b"file:///private/stale", 0.0, now).unwrap();
        let mut clock = [now, now + MAX_AGE].into_iter();
        assert_eq!(
            stale.finish(|| clock.next().unwrap(), || Ok(())),
            Err(Error::Expired)
        );
        let mut aged = Choice::directory(now);
        assert_eq!(
            aged.select(b"file:///private/aged", MAX_AGE.as_secs_f64(), now),
            Err(Error::Expired)
        );
    }
}
