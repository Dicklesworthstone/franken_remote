#![forbid(unsafe_code)]
//! One bounded local file choice, before an explicit upload gesture.
//!
//! This owner never opens or reads a file. The caller prepares a descriptor
//! outside its lock, then returns it under the original choice and connection
//! attempt. Taking a choice transfers custody once; a later authority refusal
//! must drop it rather than restoring it for a retry.

use std::time::{Duration, Instant};

pub const SELECTION_LIFETIME: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidAttempt,
    Busy,
    Stale,
    Expired,
    PreparationStarted,
    NotPreparing,
    NotPrepared,
    Exhausted,
    Clock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    AwaitingPreparation,
    Preparing,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Status {
    pub token: u64,
    pub attempt: u64,
    pub phase: Phase,
}

enum Value<T> {
    AwaitingPreparation,
    Preparing,
    Ready(T),
}

impl<T> Value<T> {
    const fn phase(&self) -> Phase {
        match self {
            Self::AwaitingPreparation => Phase::AwaitingPreparation,
            Self::Preparing => Phase::Preparing,
            Self::Ready(_) => Phase::Ready,
        }
    }
}

struct Selection<T> {
    token: u64,
    attempt: u64,
    expires: Instant,
    checked: Instant,
    value: Value<T>,
}

/// A single selected/preparing descriptor and a non-reusable token sequence.
/// The caller supplies a fresh monotonic sample after obtaining its lock.
pub struct Selections<T> {
    next_token: Option<u64>,
    selected: Option<Selection<T>>,
}

impl<T> Default for Selections<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Selections<T> {
    pub const fn new() -> Self {
        Self {
            next_token: Some(1),
            selected: None,
        }
    }

    /// Reserve one choice before showing native UI. A live earlier choice is
    /// never replaced, even when its asynchronous preparation has not returned.
    pub fn begin(&mut self, attempt: u64, now: Instant) -> Result<u64, Error> {
        if attempt == 0 {
            return Err(Error::InvalidAttempt);
        }
        if self
            .selected
            .as_ref()
            .is_some_and(|choice| now >= choice.expires)
        {
            self.selected = None;
        }
        if self.selected.is_some() {
            return Err(Error::Busy);
        }
        let token = self.next_token.ok_or(Error::Exhausted)?;
        let expires = now.checked_add(SELECTION_LIFETIME).ok_or(Error::Clock)?;
        self.next_token = token.checked_add(1);
        self.selected = Some(Selection {
            token,
            attempt,
            expires,
            checked: now,
            value: Value::AwaitingPreparation,
        });
        Ok(token)
    }

    /// Claim the original choice for exactly one off-thread preparation.
    pub fn begin_prepare(
        &mut self,
        token: u64,
        attempt: u64,
        now: Instant,
    ) -> Result<(), Error> {
        let selected = self.checked(token, attempt, now)?;
        if !matches!(selected.value, Value::AwaitingPreparation) {
            return Err(Error::PreparationStarted);
        }
        selected.value = Value::Preparing;
        Ok(())
    }

    /// Install the original prepared value. A stale, late, duplicate or
    /// unsolicited callback drops its value without replacing a newer choice.
    pub fn finish_prepare(
        &mut self,
        token: u64,
        attempt: u64,
        now: Instant,
        value: T,
    ) -> Result<(), Error> {
        let selected = self.checked(token, attempt, now)?;
        match selected.value {
            Value::Preparing => {
                selected.value = Value::Ready(value);
                Ok(())
            }
            Value::AwaitingPreparation => Err(Error::NotPreparing),
            Value::Ready(_) => Err(Error::PreparationStarted),
        }
    }

    /// Consume the matching live choice for one explicit Send. Even a
    /// premature Send consumes its token; its later preparation cannot publish
    /// into this slot. Downstream refusal never returns the value to this owner.
    pub fn take(&mut self, token: u64, attempt: u64, now: Instant) -> Result<T, Error> {
        self.checked(token, attempt, now)?;
        match self.selected.take().map(|selected| selected.value) {
            Some(Value::Ready(value)) => Ok(value),
            _ => Err(Error::NotPrepared),
        }
    }

    /// Borrow only the original live prepared value for content-free metadata.
    /// The caller must not read the file or perform native work through this
    /// borrow. Inspection does not consume or extend the choice's lifetime.
    pub fn prepared(&mut self, token: u64, attempt: u64, now: Instant) -> Result<&T, Error> {
        match &self.checked(token, attempt, now)?.value {
            Value::Ready(value) => Ok(value),
            _ => Err(Error::NotPrepared),
        }
    }

    /// Cancel only the named choice. A delayed native Cancel cannot erase its
    /// successor, including while that successor is preparing a descriptor.
    pub fn discard(&mut self, token: u64) -> bool {
        if self
            .selected
            .as_ref()
            .is_some_and(|choice| choice.token == token)
        {
            self.selected = None;
            true
        } else {
            false
        }
    }

    /// Retire all local choice custody on session end/backgrounding. Tokens
    /// remain spent, so a late callback cannot target a future choice.
    pub fn clear(&mut self) {
        self.selected = None;
    }

    #[cfg(test)]
    pub fn current(&mut self, token: u64, attempt: u64, now: Instant) -> Result<Phase, Error> {
        self.checked(token, attempt, now)
            .map(|choice| choice.value.phase())
    }

    /// Content-free state for the exact connection attempt. This is capacity
    /// and local choice evidence, never permission to send or host publication.
    pub fn status(&mut self, attempt: u64, now: Instant) -> Result<Option<Status>, Error> {
        if attempt == 0 {
            return Err(Error::InvalidAttempt);
        }
        let Some(token) = self.selected.as_ref().map(|choice| choice.token) else {
            return Ok(None);
        };
        let selected = self.checked(token, attempt, now)?;
        Ok(Some(Status {
            token,
            attempt,
            phase: selected.value.phase(),
        }))
    }

    fn checked(
        &mut self,
        token: u64,
        attempt: u64,
        now: Instant,
    ) -> Result<&mut Selection<T>, Error> {
        if attempt == 0 {
            return Err(Error::InvalidAttempt);
        }
        let selected = self.selected.as_ref().ok_or(Error::Stale)?;
        if token != selected.token || attempt != selected.attempt {
            return Err(Error::Stale);
        }
        if now >= selected.expires {
            self.selected = None;
            return Err(Error::Expired);
        }
        if now < selected.checked {
            self.selected = None;
            return Err(Error::Clock);
        }
        let selected = self.selected.as_mut().ok_or(Error::Stale)?;
        selected.checked = now;
        Ok(selected)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Owned {
        id: u8,
        dropped: Arc<AtomicUsize>,
    }

    impl Owned {
        fn new(id: u8, dropped: &Arc<AtomicUsize>) -> Self {
            Self {
                id,
                dropped: Arc::clone(dropped),
            }
        }
    }

    impl Drop for Owned {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn preparation_and_send_transfer_the_original_value_once() {
        let now = Instant::now();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut selections = Selections::new();
        let token = selections.begin(2, now).unwrap();
        assert_eq!(
            selections.current(token, 2, now),
            Ok(Phase::AwaitingPreparation)
        );
        assert_eq!(selections.begin_prepare(token, 2, now), Ok(()));
        assert_eq!(selections.current(token, 2, now), Ok(Phase::Preparing));
        assert_eq!(
            selections.finish_prepare(token, 2, now, Owned::new(4, &dropped)),
            Ok(())
        );
        assert_eq!(
            selections.status(2, now),
            Ok(Some(Status {
                token,
                attempt: 2,
                phase: Phase::Ready
            }))
        );
        let owned = selections.take(token, 2, now).unwrap();
        assert_eq!(owned.id, 4);
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        assert!(matches!(
            selections.take(token, 2, now),
            Err(Error::Stale)
        ));
        assert!(!selections.discard(token));
        drop(owned); // A downstream authority refusal drops, never requeues.
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(selections.status(2, now), Ok(None));
    }

    #[test]
    fn duplicate_or_unclaimed_preparation_never_replaces_original_custody() {
        let now = Instant::now();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut selections = Selections::new();
        let token = selections.begin(1, now).unwrap();
        assert_eq!(
            selections.finish_prepare(token, 1, now, Owned::new(1, &dropped)),
            Err(Error::NotPreparing)
        );
        selections.begin_prepare(token, 1, now).unwrap();
        assert_eq!(
            selections.begin_prepare(token, 1, now),
            Err(Error::PreparationStarted)
        );
        selections
            .finish_prepare(token, 1, now, Owned::new(2, &dropped))
            .unwrap();
        assert_eq!(
            selections.finish_prepare(token, 1, now, Owned::new(3, &dropped)),
            Err(Error::PreparationStarted)
        );
        assert_eq!(
            selections.begin_prepare(token, 1, now),
            Err(Error::PreparationStarted)
        );
        assert_eq!(selections.begin(1, now), Err(Error::Busy));
        assert_eq!(selections.begin(2, now), Err(Error::Busy));
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
        assert_eq!(selections.take(token, 1, now).unwrap().id, 2);
        assert_eq!(dropped.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cancellation_during_preparation_fences_late_callback_and_cancel() {
        let now = Instant::now();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut selections = Selections::new();
        let old = selections.begin(1, now).unwrap();
        selections.begin_prepare(old, 1, now).unwrap();
        assert!(selections.discard(old));
        let next = selections.begin(2, now).unwrap();
        assert_ne!(old, next);
        selections.begin_prepare(next, 2, now).unwrap();
        assert_eq!(
            selections.finish_prepare(old, 1, now, Owned::new(1, &dropped)),
            Err(Error::Stale)
        );
        assert!(!selections.discard(old));
        assert_eq!(selections.current(next, 2, now), Ok(Phase::Preparing));
        selections
            .finish_prepare(next, 2, now, Owned::new(2, &dropped))
            .unwrap();
        assert_eq!(selections.take(next, 2, now).unwrap().id, 2);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stale_attempt_and_invalid_identity_cannot_mutate_current_choice() {
        let now = Instant::now();
        let mut selections = Selections::new();
        assert_eq!(selections.begin(0, now), Err(Error::InvalidAttempt));
        let token = selections.begin(3, now).unwrap();
        assert_eq!(token, 1);
        for (wrong_token, attempt, expected) in [
            (token, 0, Error::InvalidAttempt),
            (token, 2, Error::Stale),
            (0, 3, Error::Stale),
            (token + 1, 3, Error::Stale),
        ] {
            assert_eq!(
                selections.begin_prepare(wrong_token, attempt, now),
                Err(expected)
            );
            assert_eq!(
                selections.finish_prepare(wrong_token, attempt, now, 7),
                Err(expected)
            );
            assert_eq!(selections.take(wrong_token, attempt, now), Err(expected));
            assert_eq!(
                selections.current(token, 3, now),
                Ok(Phase::AwaitingPreparation)
            );
        }
        assert_eq!(selections.status(2, now), Err(Error::Stale));
        selections.begin_prepare(token, 3, now).unwrap();
        selections.finish_prepare(token, 3, now, 8).unwrap();
        assert_eq!(selections.take(token, 3, now), Ok(8));
    }

    #[test]
    fn exact_expiry_is_terminal_in_every_preparation_phase() {
        let now = Instant::now();
        let until = now + SELECTION_LIFETIME;
        for phase in [
            Phase::AwaitingPreparation,
            Phase::Preparing,
            Phase::Ready,
        ] {
            let mut selections = Selections::new();
            let token = selections.begin(1, now).unwrap();
            if phase != Phase::AwaitingPreparation {
                selections
                    .begin_prepare(
                        token,
                        1,
                        until.checked_sub(Duration::from_nanos(2)).unwrap(),
                    )
                    .unwrap();
            }
            if phase == Phase::Ready {
                selections
                    .finish_prepare(
                        token,
                        1,
                        until.checked_sub(Duration::from_nanos(1)).unwrap(),
                        4,
                    )
                    .unwrap();
            }
            assert_eq!(
                selections.current(token, 1, until),
                Err(Error::Expired)
            );
            assert_eq!(selections.status(1, until), Ok(None));
            assert_eq!(
                selections.finish_prepare(token, 1, until, 5),
                Err(Error::Stale)
            );
            let next = selections.begin(1, until).unwrap();
            assert_ne!(token, next);
        }
    }

    #[test]
    fn preparation_and_status_checks_do_not_restart_the_original_lifetime() {
        let now = Instant::now();
        let until = now + SELECTION_LIFETIME;
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut selections = Selections::new();
        let token = selections.begin(1, now).unwrap();
        selections
            .begin_prepare(
                token,
                1,
                until.checked_sub(Duration::from_secs(1)).unwrap(),
            )
            .unwrap();
        assert_eq!(
            selections
                .status(1, until.checked_sub(Duration::from_nanos(1)).unwrap())
                .unwrap()
                .unwrap()
                .phase,
            Phase::Preparing
        );
        assert_eq!(
            selections.finish_prepare(token, 1, until, Owned::new(1, &dropped)),
            Err(Error::Expired)
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(selections.status(1, until), Ok(None));
    }

    #[test]
    fn taking_an_unready_choice_consumes_it_before_a_late_completion() {
        let now = Instant::now();
        for preparing in [false, true] {
            let mut selections = Selections::new();
            let token = selections.begin(1, now).unwrap();
            if preparing {
                selections.begin_prepare(token, 1, now).unwrap();
            }
            assert_eq!(
                selections.take(token, 1, now),
                Err(Error::NotPrepared)
            );
            assert_eq!(
                selections.finish_prepare(token, 1, now, 8),
                Err(Error::Stale)
            );
            assert_eq!(selections.status(1, now), Ok(None));
        }
    }

    #[test]
    fn metadata_inspection_borrows_only_the_original_live_prepared_value() {
        let now = Instant::now();
        let until = now + SELECTION_LIFETIME;
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut selections = Selections::new();
        let token = selections.begin(1, now).unwrap();
        assert!(matches!(
            selections.prepared(token, 1, now),
            Err(Error::NotPrepared)
        ));
        selections.begin_prepare(token, 1, now).unwrap();
        assert!(matches!(
            selections.prepared(token, 1, now),
            Err(Error::NotPrepared)
        ));
        selections
            .finish_prepare(token, 1, now, Owned::new(3, &dropped))
            .unwrap();
        assert!(matches!(
            selections.prepared(token, 2, now),
            Err(Error::Stale)
        ));
        assert!(matches!(
            selections.prepared(token + 1, 1, now),
            Err(Error::Stale)
        ));
        for at in [
            now,
            until.checked_sub(Duration::from_nanos(1)).unwrap(),
        ] {
            assert_eq!(
                selections.prepared(token, 1, at).map(|value| value.id),
                Ok(3)
            );
            assert_eq!(dropped.load(Ordering::SeqCst), 0);
        }
        assert!(matches!(
            selections.prepared(token, 1, until),
            Err(Error::Expired)
        ));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(matches!(
            selections.take(token, 1, until),
            Err(Error::Stale)
        ));
    }

    #[test]
    fn clearing_drops_custody_without_reusing_identity() {
        let now = Instant::now();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut selections = Selections::new();
        let token = selections.begin(1, now).unwrap();
        selections.begin_prepare(token, 1, now).unwrap();
        selections
            .finish_prepare(token, 1, now, Owned::new(1, &dropped))
            .unwrap();
        selections.clear();
        selections.clear();
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        let next = selections.begin(1, now).unwrap();
        assert_ne!(token, next);
        assert!(!selections.discard(token));
        assert_eq!(
            selections.current(next, 1, now),
            Ok(Phase::AwaitingPreparation)
        );
    }

    #[test]
    fn beginning_after_expiry_retires_the_old_descriptor() {
        let now = Instant::now();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut selections = Selections::new();
        let token = selections.begin(1, now).unwrap();
        selections.begin_prepare(token, 1, now).unwrap();
        selections
            .finish_prepare(token, 1, now, Owned::new(1, &dropped))
            .unwrap();
        let next = selections.begin(2, now + SELECTION_LIFETIME).unwrap();
        assert_ne!(token, next);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(!selections.discard(token));
    }

    #[test]
    fn an_old_sample_cannot_rejuvenate_the_matching_choice() {
        let now = Instant::now();
        let mut selections = Selections::<u8>::new();
        let token = selections.begin(1, now).unwrap();
        selections
            .begin_prepare(token, 1, now + Duration::from_secs(1))
            .unwrap();
        assert_eq!(selections.current(token, 1, now), Err(Error::Clock));
        assert_eq!(
            selections.finish_prepare(token, 1, now + Duration::from_secs(2), 1),
            Err(Error::Stale)
        );
        assert_eq!(
            selections.status(1, now + Duration::from_secs(2)),
            Ok(None)
        );
    }

    #[test]
    fn exhausted_tokens_never_wrap_even_after_clear() {
        let now = Instant::now();
        let mut selections = Selections::<u8> {
            next_token: Some(u64::MAX),
            selected: None,
        };
        let last = selections.begin(1, now).unwrap();
        assert_eq!(last, u64::MAX);
        selections.clear();
        assert_eq!(selections.begin(1, now), Err(Error::Exhausted));
        selections.clear();
        assert_eq!(selections.begin(2, now), Err(Error::Exhausted));
    }
}
