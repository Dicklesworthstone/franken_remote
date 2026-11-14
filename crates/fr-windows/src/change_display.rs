//! Original native display choices and bounded replacement-session ownership.
//! A replacement needs positive cleanup from its original session; elapsed time
//! and a user-visible successful cancellation cannot stand in for that proof.
#![forbid(unsafe_code)]

use crate::control::Choice;
use std::sync::Arc;

pub struct MenuChoice<T> {
    choice: Choice<Arc<T>>,
    lifetime: u64,
    attempt: u64,
}

pub struct Request<T> {
    pub original: Arc<T>,
    pub lifetime: u64,
    pub attempt: u64,
    /// Original message time, including the native clock's rounding margin.
    pub at: u64,
}

impl<T> MenuChoice<T> {
    pub fn new(original: Arc<T>, lifetime: u64, attempt: u64, opened: u64) -> Self {
        Self {
            choice: Choice::new(original, opened),
            lifetime,
            attempt,
        }
    }

    /// Spend the choice even on refusal. A popup from an earlier owner or
    /// native mapping or connection attempt cannot close the current owner or
    /// authorize another dial. An internal reconnect retains the same Arc.
    pub fn take(
        &mut self,
        current: &Arc<T>,
        lifetime: u64,
        attempt: u64,
        available: bool,
        message: u32,
        now: u64,
    ) -> Option<Request<T>> {
        let (original, age) = self.choice.take(message, now)?;
        if !available
            || self.lifetime != lifetime
            || self.attempt != attempt
            || !Arc::ptr_eq(&original, current)
        {
            return None;
        }
        Some(Request {
            original,
            lifetime,
            attempt,
            at: now.checked_sub(u64::try_from(age.as_millis()).ok()?)?,
        })
    }
}

/// One invocation can open at most this many fresh native session owners.
/// Each owner's existing bounded connector retry policy remains unchanged.
pub const MAX_OWNERS: u8 = 32;
pub const REPLACEMENT_MS: u64 = 10_000;
pub const REDIAL_MS: u64 = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Resize,
    Display,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Busy,
    Limit,
    Cancelled,
    Clock,
    Expired,
    Cleanup,
    Start,
}

impl Refusal {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Busy => "replacement_pending",
            Self::Limit => "replacement_limit",
            Self::Cancelled => "replacement_cancelled",
            Self::Clock => "replacement_clock_invalid",
            Self::Expired => "replacement_expired",
            Self::Cleanup => "original_cleanup_unconfirmed",
            Self::Start => "replacement_start_failed",
        }
    }
}

/// A ready replacement retains its original bound through subsequent native
/// queries. Its final check belongs immediately before starting the new owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opening {
    pub cause: Cause,
    pub choose_display: bool,
    checked_at: u64,
    until: u64,
}

impl Opening {
    pub fn check(self, now: u64) -> Result<(), Refusal> {
        if now < self.checked_at {
            Err(Refusal::Clock)
        } else if now >= self.until {
            Err(Refusal::Expired)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    None,
    Waiting,
    Open(Opening),
    Refused(Refusal),
}

struct Pending {
    cause: Cause,
    lifetime: u64,
    until: u64,
    last: u64,
    ready_at: Option<u64>,
}

pub struct Replacements {
    opened: u8,
    /// After an explicit switch, every replacement reads a new catalog and
    /// asks again instead of silently reverting to an initial CLI handle.
    choose_display: bool,
    pending: Option<Pending>,
}

impl Default for Replacements {
    fn default() -> Self {
        Self {
            opened: 1,
            choose_display: false,
            pending: None,
        }
    }
}

impl Replacements {
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn available(&self) -> bool {
        !self.pending() && self.opened < MAX_OWNERS
    }

    pub fn request(&mut self, cause: Cause, lifetime: u64, at: u64) -> Result<(), Refusal> {
        if self.pending() {
            return Err(Refusal::Busy);
        }
        if self.opened >= MAX_OWNERS {
            return Err(Refusal::Limit);
        }
        self.pending = Some(Pending {
            cause,
            lifetime,
            until: at.checked_add(REPLACEMENT_MS).ok_or(Refusal::Clock)?,
            last: at,
            ready_at: None,
        });
        Ok(())
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    /// Poll the original owner's actual cleanup evidence. The clock includes
    /// suspend. Repeated polls never extend either the original bound or the
    /// serial listener's cooldown; refusal consumes this replacement request.
    pub fn poll(
        &mut self,
        now: u64,
        lifetime: u64,
        visible: bool,
        finished: bool,
        orderly: bool,
    ) -> Action {
        let Some(pending) = &mut self.pending else {
            return Action::None;
        };
        let refusal =
            if !visible || (pending.cause == Cause::Display && lifetime != pending.lifetime) {
                Some(Refusal::Cancelled)
            } else if now < pending.last {
                Some(Refusal::Clock)
            } else if now >= pending.until {
                Some(Refusal::Expired)
            } else if finished && !orderly {
                Some(Refusal::Cleanup)
            } else {
                None
            };
        if let Some(refusal) = refusal {
            self.pending = None;
            return Action::Refused(refusal);
        }
        pending.last = now;
        if !finished {
            return Action::Waiting;
        }
        let ready = pending
            .ready_at
            .get_or_insert(now.saturating_add(REDIAL_MS));
        if now < *ready {
            return Action::Waiting;
        }
        let cause = pending.cause;
        let until = pending.until;
        self.pending = None;
        self.opened += 1; // request refuses at MAX_OWNERS, well before overflow
        self.choose_display |= cause == Cause::Display;
        Action::Open(Opening {
            cause,
            choose_display: self.choose_display,
            checked_at: now,
            until,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_choice_keeps_the_exact_owner_native_lifetime_and_message_age() {
        let original = Arc::new(7);
        let mut choice = MenuChoice::new(Arc::clone(&original), 3, 9, 1_000);
        let request = choice.take(&original, 3, 9, true, 1_020, 1_040).unwrap();
        assert!(Arc::ptr_eq(&original, &request.original));
        assert_eq!(request.lifetime, 3);
        assert_eq!(request.attempt, 9);
        assert_eq!(request.at, 1_004); // 1020 minus the 16 ms rounding margin
        assert!(choice.take(&original, 3, 9, true, 1_050, 1_050).is_none());
    }

    #[test]
    fn a_stale_or_unavailable_popup_cannot_close_a_replacement_owner() {
        let original = Arc::new(7);
        let replacement = Arc::new(7);
        for (current, lifetime, available) in [
            (&replacement, 3, true),
            (&original, 4, true),
            (&original, 3, false),
        ] {
            let mut choice = MenuChoice::new(Arc::clone(&original), 3, 9, 1_000);
            assert!(
                choice
                    .take(current, lifetime, 9, available, 1_020, 1_040)
                    .is_none()
            );
            assert!(choice.take(&original, 3, 9, true, 1_050, 1_050).is_none());
        }
    }

    #[test]
    fn stale_and_same_tick_native_commands_never_start_a_replacement_budget() {
        let original = Arc::new(());
        for (message, now) in [(999, 1_020), (1_000, 1_020), (1_020, 1_104), (1_050, 1_040)] {
            let mut choice = MenuChoice::new(Arc::clone(&original), 1, 9, 1_000);
            assert!(choice.take(&original, 1, 9, true, message, now).is_none());
        }
    }

    #[test]
    fn an_internal_reconnect_cannot_redeem_the_same_owners_old_popup() {
        let original = Arc::new(());
        let mut choice = MenuChoice::new(Arc::clone(&original), 3, 9, 1_000);
        // Native owner and mapping are unchanged, but the shared session has
        // started another connection attempt. Even a fresh event is refused.
        assert!(choice.take(&original, 3, 10, true, 1_020, 1_040).is_none());
        // Refusal consumes the choice; no later snapshot can revive it.
        assert!(choice.take(&original, 3, 9, true, 1_050, 1_050).is_none());
        let mut fresh = MenuChoice::new(Arc::clone(&original), 3, 10, 1_100);
        let request = fresh.take(&original, 3, 10, true, 1_120, 1_140).unwrap();
        assert_eq!(request.attempt, 10);
        assert!(Arc::ptr_eq(&original, &request.original));
    }

    #[test]
    fn positive_original_cleanup_precedes_the_full_listener_cooldown() {
        let mut replacements = Replacements::default();
        replacements.request(Cause::Display, 3, 1_000).unwrap();
        assert_eq!(
            replacements.poll(1_100, 3, true, false, false),
            Action::Waiting
        );
        assert_eq!(
            replacements.poll(1_200, 3, true, true, true),
            Action::Waiting
        );
        assert_eq!(
            replacements.poll(1_599, 3, true, true, true),
            Action::Waiting
        );
        assert_eq!(
            replacements.poll(1_600, 3, true, true, true),
            Action::Open(Opening {
                cause: Cause::Display,
                choose_display: true,
                checked_at: 1_600,
                until: 11_000,
            })
        );
        assert_eq!(replacements.poll(1_700, 3, true, true, true), Action::None);
    }

    #[test]
    fn cancellation_or_failed_cleanup_never_becomes_success_by_waiting() {
        for (lifetime, visible, finished, orderly, refused) in [
            (4, true, true, true, Refusal::Cancelled),
            (3, false, true, true, Refusal::Cancelled),
            (3, true, true, false, Refusal::Cleanup),
        ] {
            let mut replacements = Replacements::default();
            replacements.request(Cause::Display, 3, 1_000).unwrap();
            assert_eq!(
                replacements.poll(1_100, lifetime, visible, finished, orderly),
                Action::Refused(refused)
            );
            assert_eq!(replacements.poll(2_000, 3, true, true, true), Action::None);
        }
        let mut replacements = Replacements::default();
        replacements.request(Cause::Display, 3, 1_000).unwrap();
        replacements.cancel();
        assert_eq!(replacements.poll(2_000, 3, true, true, true), Action::None);
    }

    #[test]
    fn the_original_budget_cannot_restart_after_sleep_or_late_cleanup() {
        for finished in [false, true] {
            let mut replacements = Replacements::default();
            replacements.request(Cause::Display, 3, 1_000).unwrap();
            assert_eq!(
                replacements.poll(10_900, 3, true, finished, true),
                Action::Waiting
            );
            assert_eq!(
                replacements.poll(11_000, 3, true, true, true),
                Action::Refused(Refusal::Expired)
            );
        }
        let mut replacements = Replacements::default();
        replacements.request(Cause::Display, 3, 1_000).unwrap();
        assert_eq!(
            replacements.poll(999, 3, true, true, true),
            Action::Refused(Refusal::Clock)
        );
        assert_eq!(
            replacements.request(Cause::Display, 3, u64::MAX - 1),
            Err(Refusal::Clock)
        );
    }

    #[test]
    fn later_resize_never_restores_a_cli_display_after_a_manual_switch() {
        let mut replacements = Replacements::default();
        for (cause, at, choose) in [
            (Cause::Resize, 1_000, false),
            (Cause::Display, 2_000, true),
            (Cause::Resize, 3_000, true),
        ] {
            replacements.request(cause, 3, at).unwrap();
            assert_eq!(replacements.request(cause, 3, at), Err(Refusal::Busy));
            assert_eq!(replacements.poll(at, 3, true, true, true), Action::Waiting);
            assert_eq!(
                replacements.poll(at + REDIAL_MS, 3, true, true, true),
                Action::Open(Opening {
                    cause,
                    choose_display: choose,
                    checked_at: at + REDIAL_MS,
                    until: at + REPLACEMENT_MS,
                })
            );
        }
    }

    #[test]
    fn a_ready_opening_retains_the_original_bound_across_native_waits() {
        let mut replacements = Replacements::default();
        replacements.request(Cause::Display, 3, 1_000).unwrap();
        assert_eq!(
            replacements.poll(10_000, 3, true, true, true),
            Action::Waiting
        );
        let Action::Open(opening) = replacements.poll(10_400, 3, true, true, true) else {
            panic!("the original cleanup and cooldown completed");
        };
        assert_eq!(opening.check(10_400), Ok(()));
        assert_eq!(opening.check(10_999), Ok(()));
        // A later clipboard lock or native window-size query cannot give this
        // already-ready opening another retirement/cooldown lifetime.
        assert_eq!(opening.check(11_000), Err(Refusal::Expired));
        assert_eq!(opening.check(30_000), Err(Refusal::Expired));
        assert_eq!(opening.check(10_399), Err(Refusal::Clock));
        assert_eq!(replacements.poll(10_500, 3, true, true, true), Action::None);
    }

    #[test]
    fn one_command_has_a_finite_fresh_owner_budget() {
        let mut replacements = Replacements::default();
        for index in 1..MAX_OWNERS {
            let at = u64::from(index) * 1_000;
            assert!(replacements.available());
            replacements.request(Cause::Display, 3, at).unwrap();
            assert_eq!(replacements.poll(at, 3, true, true, true), Action::Waiting);
            assert!(matches!(
                replacements.poll(at + REDIAL_MS, 3, true, true, true),
                Action::Open(Opening {
                    choose_display: true,
                    ..
                })
            ));
        }
        assert!(!replacements.available());
        assert_eq!(
            replacements.request(Cause::Display, 3, 50_000),
            Err(Refusal::Limit)
        );
    }
}
