//! Original native viewing choices and bounded replacement-session ownership.
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

impl<T> Request<T> {
    pub fn current(&self, original: &Arc<T>, lifetime: u64, attempt: u64) -> bool {
        Arc::ptr_eq(&self.original, original)
            && self.lifetime == lifetime
            && self.attempt == attempt
    }
}

/// A native foreground notification, scoped before any deferred UI work.
/// Current foreground ownership must also be checked by the native caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    pub lifetime: u64,
    pub attempt: u64,
    pub at: u64,
}

impl Activation {
    pub fn current(self, scope: Option<(u64, u64)>, now: u64) -> bool {
        scope == Some((self.lifetime, self.attempt))
            && now >= self.at
            && now - self.at < ACTIVATION_MS
    }
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

pub const REPLACEMENT_MS: u64 = 10_000;
pub const REDIAL_MS: u64 = 400;
/// The longest wait between two reconnects while no picture arrives.
pub const MAX_RECONNECT_DELAY_MS: u64 = 5_000;

/// How long to wait before reconnecting after `failures` reconnects in a row
/// that showed no picture: 0.4, 0.8, 1.6, 3.2 s, then every 5 s, forever.
pub fn reconnect_delay(failures: u32) -> u64 {
    REDIAL_MS
        .saturating_mul(1 << failures.min(4))
        .min(MAX_RECONNECT_DELAY_MS)
}
/// A foreground callback cannot wait behind native work and then become a new
/// viewing request. This is admission age; accepted requests keep their own
/// original ten-second opening bound.
pub const ACTIVATION_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cause {
    Resize,
    Display,
    /// End the original input owner and open fresh viewing after actual cleanup.
    StopControl,
    /// Retire control on focus loss, then wait without a connection for a new
    /// foreground event before opening fresh viewing.
    FocusLost,
    /// The session ended or failed without the user closing the window. A
    /// window never gives up: once the old session has finished it opens a
    /// fresh one (after a backoff, and only while the window is shown), with
    /// no limit on how many times, and control resumes by itself.
    Reconnect,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Busy,
    Cancelled,
    Clock,
    Expired,
    Cleanup,
    Presentation,
    Start,
}

impl Refusal {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Busy => "replacement_pending",
            Self::Cancelled => "replacement_cancelled",
            Self::Clock => "replacement_clock_invalid",
            Self::Expired => "replacement_expired",
            Self::Cleanup => "original_cleanup_unconfirmed",
            Self::Presentation => "retired_picture_clear_failed",
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
    /// Original service and cleanup succeeded; no foreground request exists.
    Dormant,
    Open(Opening),
    Refused(Refusal),
}

struct Pending {
    cause: Cause,
    lifetime: u64,
    until: u64,
    last: u64,
    ready_at: Option<u64>,
    /// A fresh foreground event is a new viewing choice, never an extension of
    /// the original cleanup. Once accepted, repeats cannot refresh its bound.
    foreground: Option<u64>,
    /// The wait after the old session finished before dialing again (at
    /// least the host listener's cooldown, `REDIAL_MS`).
    delay: u64,
}

pub struct Replacements {
    /// Fresh session owners this window has opened (diagnostic only: there
    /// is no limit, so a window can reconnect for as long as it is open).
    opened: u32,
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
        !self.pending()
    }

    /// Fresh session owners opened so far.
    pub fn opened(&self) -> u32 {
        self.opened
    }

    /// A reconnect is waiting for the old session or its backoff.
    pub fn reconnecting(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.cause == Cause::Reconnect)
    }

    pub fn request(&mut self, cause: Cause, lifetime: u64, at: u64) -> Result<(), Refusal> {
        if self.pending() {
            return Err(Refusal::Busy);
        }
        self.pending = Some(Pending {
            cause,
            lifetime,
            until: at.checked_add(REPLACEMENT_MS).ok_or(Refusal::Clock)?,
            last: at,
            ready_at: None,
            foreground: None,
            delay: REDIAL_MS,
        });
        Ok(())
    }

    /// Replace whatever is pending with a reconnect: once the old session has
    /// finished and `delay_ms` (at least `REDIAL_MS`) has passed, and while
    /// the window is shown, `poll` opens a fresh session. It never expires
    /// and is never refused; only the user's close cancels it.
    pub fn reconnect(&mut self, lifetime: u64, at: u64, delay_ms: u64) {
        self.pending = Some(Pending {
            cause: Cause::Reconnect,
            lifetime,
            until: u64::MAX,
            last: at,
            ready_at: None,
            foreground: None,
            delay: delay_ms.max(REDIAL_MS),
        });
    }

    pub fn cancel(&mut self) {
        self.pending = None;
    }

    pub fn focus_pending(&self) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| pending.cause == Cause::FocusLost)
    }

    pub fn dormant(&self) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.cause == Cause::FocusLost
                && pending.ready_at.is_some()
                && pending.foreground.is_none()
        })
    }

    /// Admit one fresh, already scoped native foreground event. Cleanup still
    /// has its original bound; only positively completed cleanup may wait in a
    /// dormant window. The invocation's close/deadline must cancel this owner
    /// before calling this method or polling it again.
    pub fn resume_focus(&mut self, activation: Activation, now: u64) -> Result<(), Refusal> {
        let Some(pending) = &mut self.pending else {
            return Err(Refusal::Cancelled);
        };
        if pending.cause != Cause::FocusLost || activation.lifetime != pending.lifetime {
            return Err(Refusal::Cancelled);
        }
        if now < pending.last || now < activation.at {
            return Err(Refusal::Clock);
        }
        if now - activation.at >= ACTIVATION_MS
            || (pending.ready_at.is_none() && now >= pending.until)
        {
            return Err(Refusal::Expired);
        }
        if pending.foreground.is_none() {
            pending.foreground = Some(
                activation
                    .at
                    .checked_add(REPLACEMENT_MS)
                    .ok_or(Refusal::Clock)?,
            );
        }
        pending.last = now;
        Ok(())
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
        if pending.cause == Cause::Reconnect {
            // Never refused and never expired: wait for the old session to
            // finish, the backoff to pass and the window to be shown.
            pending.last = pending.last.max(now);
            if !finished {
                return Action::Waiting;
            }
            let ready = *pending
                .ready_at
                .get_or_insert(now.saturating_add(pending.delay));
            if !visible || now < ready {
                return Action::Waiting;
            }
            self.pending = None;
            self.opened = self.opened.saturating_add(1);
            return Action::Open(Opening {
                cause: Cause::Reconnect,
                choose_display: self.choose_display,
                checked_at: now,
                until: now.saturating_add(REPLACEMENT_MS),
            });
        }
        let focused = pending.cause == Cause::FocusLost;
        let waiting_for_foreground = focused && pending.foreground.is_none();
        let deadline = if focused && pending.ready_at.is_some() {
            pending.foreground
        } else {
            Some(pending.until)
        };
        let refusal = if (!visible && !waiting_for_foreground)
            || (pending.cause != Cause::Resize && lifetime != pending.lifetime)
        {
            Some(Refusal::Cancelled)
        } else if now < pending.last {
            Some(Refusal::Clock)
        } else if deadline.is_some_and(|until| now >= until) {
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
            .get_or_insert(now.saturating_add(pending.delay));
        if waiting_for_foreground {
            return Action::Dormant;
        }
        if now < *ready {
            return Action::Waiting;
        }
        let cause = pending.cause;
        let until = pending.foreground.unwrap_or(pending.until);
        self.pending = None;
        self.opened = self.opened.saturating_add(1);
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
        for cause in [Cause::Display, Cause::StopControl] {
            for (lifetime, visible, finished, orderly, refused) in [
                (4, true, true, true, Refusal::Cancelled),
                (3, false, true, true, Refusal::Cancelled),
                (3, true, true, false, Refusal::Cleanup),
            ] {
                let mut replacements = Replacements::default();
                replacements.request(cause, 3, 1_000).unwrap();
                assert_eq!(
                    replacements.poll(1_100, lifetime, visible, finished, orderly),
                    Action::Refused(refused)
                );
                assert_eq!(replacements.poll(2_000, 3, true, true, true), Action::None);
            }
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
            (Cause::StopControl, 2_000, false),
            (Cause::Display, 3_000, true),
            (Cause::StopControl, 4_000, true),
            (Cause::Resize, 5_000, true),
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
    fn stopping_control_needs_original_cleanup_and_cannot_renew_the_budget() {
        let mut replacements = Replacements::default();
        replacements.request(Cause::StopControl, 3, 1_000).unwrap();
        assert_eq!(
            replacements.poll(1_100, 3, true, false, false),
            Action::Waiting
        );
        assert_eq!(
            replacements.poll(1_200, 3, true, true, true),
            Action::Waiting
        );
        let Action::Open(opening) = replacements.poll(1_600, 3, true, true, true) else {
            panic!("successful original close permits fresh viewing");
        };
        assert_eq!(opening.cause, Cause::StopControl);
        assert!(!opening.choose_display);
        assert_eq!(opening.check(10_999), Ok(()));
        assert_eq!(opening.check(11_000), Err(Refusal::Expired));
        assert_eq!(replacements.poll(1_700, 3, true, true, true), Action::None);

        let mut late = Replacements::default();
        late.request(Cause::StopControl, 3, 1_000).unwrap();
        assert_eq!(late.poll(10_900, 3, true, true, true), Action::Waiting);
        assert_eq!(
            late.poll(11_000, 3, true, true, true),
            Action::Refused(Refusal::Expired)
        );
        assert_eq!(late.poll(11_400, 3, true, true, true), Action::None);
    }

    #[test]
    fn a_window_can_open_fresh_owners_without_limit() {
        let mut replacements = Replacements::default();
        for index in 1..500_u64 {
            let at = index * 1_000;
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
        assert!(replacements.available());
        assert_eq!(replacements.opened(), 500);
    }

    #[test]
    fn a_reconnect_waits_for_the_old_session_then_its_backoff() {
        let mut replacements = Replacements::default();
        replacements.reconnect(3, 1_000, 1_600);
        assert!(replacements.reconnecting());
        // The old session is still closing: no dial, however long it takes.
        assert_eq!(
            replacements.poll(90_000, 3, true, false, false),
            Action::Waiting
        );
        // Finished (even uncleanly): the backoff starts now.
        assert_eq!(
            replacements.poll(90_100, 3, true, true, false),
            Action::Waiting
        );
        assert_eq!(
            replacements.poll(91_699, 3, true, true, false),
            Action::Waiting
        );
        assert!(matches!(
            replacements.poll(91_700, 3, true, true, false),
            Action::Open(Opening {
                cause: Cause::Reconnect,
                ..
            })
        ));
        assert!(!replacements.reconnecting());
    }

    #[test]
    fn a_reconnect_is_never_refused_and_waits_while_the_window_is_hidden() {
        let mut replacements = Replacements::default();
        replacements.reconnect(3, 1_000, 0);
        // Hidden, a new native lifetime, a failed cleanup, hours later:
        // still waiting, never refused.
        for now in [1_100, 50_000, 10_000_000] {
            assert_eq!(
                replacements.poll(now, 9, false, true, false),
                Action::Waiting
            );
        }
        // Shown again: open at once (the listener cooldown passed long ago).
        let Action::Open(opening) = replacements.poll(10_000_001, 9, true, true, false) else {
            panic!("a shown window reconnects");
        };
        assert_eq!(opening.cause, Cause::Reconnect);
        assert_eq!(opening.check(10_000_001), Ok(()));
    }

    #[test]
    fn a_reconnect_replaces_any_pending_replacement() {
        let mut replacements = Replacements::default();
        replacements.request(Cause::Resize, 3, 1_000).unwrap();
        replacements.reconnect(3, 1_100, REDIAL_MS);
        assert!(replacements.reconnecting());
        assert_eq!(
            replacements.poll(1_200, 3, true, true, false),
            Action::Waiting
        );
        assert!(matches!(
            replacements.poll(1_200 + REDIAL_MS, 3, true, true, false),
            Action::Open(Opening {
                cause: Cause::Reconnect,
                ..
            })
        ));
    }

    #[test]
    fn reconnect_backoff_grows_to_five_seconds_and_stays() {
        let delays: Vec<u64> = (0..8).map(reconnect_delay).collect();
        assert_eq!(delays, [400, 800, 1_600, 3_200, 5_000, 5_000, 5_000, 5_000]);
        assert_eq!(reconnect_delay(u32::MAX), MAX_RECONNECT_DELAY_MS);
    }

    fn foreground(at: u64) -> Activation {
        Activation {
            lifetime: 3,
            attempt: 9,
            at,
        }
    }

    #[test]
    fn a_focus_stop_waits_for_actual_cleanup_then_a_fresh_foreground_event() {
        let mut replacements = Replacements::default();
        replacements.request(Cause::FocusLost, 3, 1_000).unwrap();
        assert_eq!(
            replacements.poll(1_100, 3, false, false, false),
            Action::Waiting
        );
        assert_eq!(
            replacements.poll(1_200, 3, false, true, true),
            Action::Dormant
        );
        // Time in another app is not a renewed cleanup or an implicit dial.
        assert_eq!(
            replacements.poll(90_000, 3, false, true, true),
            Action::Dormant
        );
        assert_eq!(
            replacements.poll(91_000, 3, true, true, true),
            Action::Dormant
        );
        replacements
            .resume_focus(foreground(92_000), 92_010)
            .unwrap();
        let Action::Open(opening) = replacements.poll(92_010, 3, true, true, true) else {
            panic!("a new foreground choice follows positive original cleanup");
        };
        assert_eq!(opening.cause, Cause::FocusLost);
        assert_eq!(opening.check(101_999), Ok(()));
        assert_eq!(opening.check(102_000), Err(Refusal::Expired));
        assert_eq!(
            replacements.poll(102_010, 3, true, true, true),
            Action::None
        );
    }

    #[test]
    fn returning_while_original_cleanup_runs_keeps_both_original_deadlines() {
        let mut replacements = Replacements::default();
        replacements.request(Cause::FocusLost, 3, 1_000).unwrap();
        replacements.resume_focus(foreground(1_100), 1_110).unwrap();
        assert_eq!(
            replacements.poll(1_110, 3, true, false, false),
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
        let Action::Open(opening) = replacements.poll(1_600, 3, true, true, true) else {
            panic!("original cleanup and listener cooldown completed");
        };
        assert_eq!(opening.check(11_099), Ok(()));
        assert_eq!(opening.check(11_100), Err(Refusal::Expired));
    }

    #[test]
    fn failed_or_late_focus_cleanup_never_becomes_dormant_eligible() {
        for (now, orderly, expected) in [
            (1_100, false, Refusal::Cleanup),
            (11_000, true, Refusal::Expired),
            (99_000, true, Refusal::Expired),
        ] {
            let mut replacements = Replacements::default();
            replacements.request(Cause::FocusLost, 3, 1_000).unwrap();
            assert_eq!(
                replacements.poll(now, 3, false, true, orderly),
                Action::Refused(expected)
            );
            assert_eq!(
                replacements.resume_focus(foreground(now + 1), now + 2),
                Err(Refusal::Cancelled)
            );
            assert_eq!(
                replacements.poll(now + 3, 3, true, true, true),
                Action::None
            );
        }
    }

    #[test]
    fn foreground_scope_and_age_reject_old_owner_attempts_and_late_callbacks() {
        let activation = foreground(1_000);
        assert!(activation.current(Some((3, 9)), 1_000));
        assert!(activation.current(Some((3, 9)), 1_249));
        for (scope, now) in [
            (Some((4, 9)), 1_010),
            (Some((3, 10)), 1_010),
            (None, 1_010),
            (Some((3, 9)), 999),
            (Some((3, 9)), 1_250),
        ] {
            assert!(!activation.current(scope, now));
        }
        let original = Arc::new(());
        let request = Request {
            original: Arc::clone(&original),
            lifetime: 3,
            attempt: 9,
            at: 1_000,
        };
        assert!(request.current(&original, 3, 9));
        assert!(!request.current(&Arc::new(()), 3, 9));
        assert!(!request.current(&original, 4, 9));
        assert!(!request.current(&original, 3, 10));
    }

    #[test]
    fn a_repeated_foreground_notification_never_rearms_the_viewing_deadline() {
        let mut replacements = Replacements::default();
        replacements.request(Cause::FocusLost, 3, 1_000).unwrap();
        assert_eq!(
            replacements.poll(1_100, 3, false, true, true),
            Action::Dormant
        );
        replacements
            .resume_focus(foreground(20_000), 20_010)
            .unwrap();
        replacements
            .resume_focus(foreground(29_999), 29_999)
            .unwrap();
        assert_eq!(
            replacements.poll(30_000, 3, true, true, true),
            Action::Refused(Refusal::Expired)
        );
        assert_eq!(replacements.poll(30_001, 3, true, true, true), Action::None);
    }

    #[test]
    fn close_invocation_expiry_and_new_focus_loss_cancel_dormant_recovery() {
        for local_close in [true, false] {
            let mut replacements = Replacements::default();
            replacements.request(Cause::FocusLost, 3, 1_000).unwrap();
            assert_eq!(
                replacements.poll(1_100, 3, false, true, true),
                Action::Dormant
            );
            // The pump cancels for both a native close and --seconds expiry.
            if local_close {
                replacements.cancel();
                assert_eq!(
                    replacements.resume_focus(foreground(2_000), 2_001),
                    Err(Refusal::Cancelled)
                );
            } else {
                replacements.resume_focus(foreground(2_000), 2_001).unwrap();
                assert_eq!(
                    replacements.poll(2_002, 4, false, true, true),
                    Action::Refused(Refusal::Cancelled)
                );
            }
            assert_eq!(replacements.poll(3_000, 3, true, true, true), Action::None);
        }
    }

    #[test]
    fn repeated_focus_cycles_never_run_out_of_owners() {
        let mut replacements = Replacements::default();
        for index in 1..100_u64 {
            let at = index * 100_000;
            replacements.request(Cause::FocusLost, 3, at).unwrap();
            assert_eq!(
                replacements.poll(at + 1, 3, false, true, true),
                Action::Dormant
            );
            replacements
                .resume_focus(foreground(at + 50_000), at + 50_001)
                .unwrap();
            assert!(matches!(
                replacements.poll(at + 50_001, 3, true, true, true),
                Action::Open(Opening {
                    cause: Cause::FocusLost,
                    ..
                })
            ));
        }
        assert_eq!(
            replacements.request(Cause::FocusLost, 3, 40_000_000),
            Ok(())
        );
    }
}
