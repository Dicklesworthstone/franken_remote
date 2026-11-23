//! Fixed-memory custody of shortcut keys between the native hook and UI.
//!
//! Win32 messages carry only a never-reused ordinal. The original key time,
//! capture scope and bounded ordered queue stay here. No borrowed pointer or
//! key content travels in a posted window message. A failed post or lost
//! ordered event fences this queue until its original owner handles failure.
#![forbid(unsafe_code)]

use super::{KeyMessage, MAX_DEFERRED, captured_age, synthetic_ctrl};

#[derive(Clone, Copy)]
struct Entry {
    ordinal: usize,
    key: KeyMessage,
    at: u64,
}

/// No Debug: physical key contents are private input state.
pub struct Delivery<S> {
    pub original: S,
    pub key: KeyMessage,
    pub at: u64,
    pub synthetic_control: bool,
}

/// At most 64 key transitions, including releases; no per-event allocation.
/// `S` names the original native session/connection attempt, never authority.
pub struct Mailbox<S> {
    original: Option<S>,
    opened: u64,
    entries: [Option<Entry>; MAX_DEFERRED],
    /// Posted notifications keep credit even after their keys were retired.
    /// Repeated focus changes cannot grow the hidden Win32 message queue.
    notifications: [Option<usize>; MAX_DEFERRED],
    head: usize,
    len: usize,
    next: Option<usize>,
    failure: Option<S>,
}

impl<S: Clone + PartialEq> Default for Mailbox<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Clone + PartialEq> Mailbox<S> {
    pub const fn new() -> Self {
        Self {
            original: None,
            opened: 0,
            entries: [None; MAX_DEFERRED],
            notifications: [None; MAX_DEFERRED],
            head: 0,
            len: 0,
            next: Some(1),
            failure: None,
        }
    }

    /// Called only after the native owner has handled the preceding focus or
    /// session transition. Re-enabling the same owner starts a new native
    /// capture interval; a delayed hook callback from before it cannot enter.
    pub fn activate(&mut self, original: Option<S>, now: u64) {
        if self.failure.is_some() || self.original == original {
            return;
        }
        self.clear();
        self.original = original;
        self.opened = now;
    }

    /// Retire pending notifications before local focus/close cleanup. Never
    /// erase a failed original owner's report or reset notification ordinals.
    pub fn deactivate(&mut self) {
        self.original = None;
        self.clear();
    }

    pub fn active(&self) -> bool {
        self.original.is_some() && self.failure.is_none()
    }

    /// The hook thread has joined and its native window is gone. No earlier
    /// notification can be delivered; keep the ordinal floor for future windows.
    pub fn retired(&mut self) {
        self.deactivate();
        self.notifications.fill(None);
        self.failure = None;
    }

    /// Keep the hook's ORIGINAL timestamp. `now` is the suspend-inclusive
    /// native tick sampled before queuing; dispatch and posting cannot extend it.
    pub fn push(&mut self, key: KeyMessage, now: u64) -> Option<usize> {
        self.original.as_ref()?;
        if self.failure.is_some() {
            return None;
        }
        let low = u32::try_from(now & u64::from(u32::MAX)).ok()?;
        let age = u64::from(low.wrapping_sub(key.time));
        let Some(at) = now.checked_sub(age) else {
            self.fail();
            return None;
        };
        // Future wrapping timestamps and a regressed clock cannot silently
        // drop an ordered release. A hook callback cannot legitimately wait
        // half of Win32's 32-bit clock domain before dispatch.
        if age > u64::from(i32::MAX.cast_unsigned()) || now < self.opened {
            self.fail();
            return None;
        }
        // A coarse same-tick timestamp cannot prove the input happened after
        // activation. Such input never belonged to this new capture interval.
        if at <= self.opened {
            return None;
        }
        if captured_age(at, now).is_err() {
            self.fail();
            return None;
        }
        if self.len == MAX_DEFERRED {
            self.fail();
            return None;
        }
        let Some(credit) = self.notifications.iter().position(Option::is_none) else {
            self.fail();
            return None;
        };
        let Some(ordinal) = self.next else {
            self.fail();
            return None;
        };
        self.next = ordinal.checked_add(1);
        let tail = (self.head + self.len) % MAX_DEFERRED;
        self.entries[tail] = Some(Entry { ordinal, key, at });
        self.notifications[credit] = Some(ordinal);
        self.len += 1;
        Some(ordinal)
    }

    /// A notification consumes its exact original entry once. An old/duplicate
    /// notification is inert; missing a predecessor is terminal for the owner.
    pub fn take(&mut self, ordinal: usize, now: u64) -> Option<Delivery<S>> {
        let notification = self
            .notifications
            .iter_mut()
            .find(|value| **value == Some(ordinal))?;
        *notification = None;
        if self.failure.is_some() {
            return None;
        }
        let original = self.original.as_ref()?;
        let first = self.entries[self.head]?;
        if ordinal < first.ordinal {
            return None;
        }
        if ordinal != first.ordinal || captured_age(first.at, now).is_err() {
            self.fail();
            return None;
        }
        let next = if self.len > 1 {
            self.entries[(self.head + 1) % MAX_DEFERRED].map(|entry| entry.key)
        } else {
            None
        };
        let result = Delivery {
            original: original.clone(),
            key: first.key,
            at: first.at,
            synthetic_control: synthetic_ctrl(first.key, next),
        };
        self.entries[self.head] = None;
        self.head = (self.head + 1) % MAX_DEFERRED;
        self.len -= 1;
        Some(result)
    }

    /// A failed post releases only its reserved notification credit. A late
    /// post failure from a retired interval cannot fence a replacement owner.
    pub fn post_failed(&mut self, ordinal: usize) {
        for value in &mut self.notifications {
            if *value == Some(ordinal) {
                *value = None;
            }
        }
        if self
            .entries
            .iter()
            .flatten()
            .any(|entry| entry.ordinal == ordinal)
        {
            self.fail();
        }
    }

    /// Posting failure, overflow or a lost UI delivery has one original owner.
    /// Capture that owner before clearing queued keys, and do not admit more.
    pub fn fail(&mut self) {
        if self.failure.is_none() {
            self.failure = self.original.take();
        }
        self.clear();
    }

    /// The caller fences this exact owner before trying to reactivate input.
    pub fn take_failure(&mut self) -> Option<S> {
        self.failure.take()
    }

    fn clear(&mut self) {
        self.entries.fill(None);
        self.head = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(scan: u8, extended: bool, pressed: bool, time: u32) -> KeyMessage {
        KeyMessage {
            scan,
            extended,
            pressed,
            time,
        }
    }

    fn opened_mailbox() -> Mailbox<u64> {
        let mut mailbox = Mailbox::new();
        mailbox.activate(Some(1), 900);
        mailbox
    }

    #[test]
    fn hook_delay_and_ui_delay_share_the_original_exclusive_deadline() {
        let mut mailbox = opened_mailbox();
        let ordinal = mailbox.push(key(0x1e, false, true, 1_000), 1_060).unwrap();
        let delivered = mailbox.take(ordinal, 1_083).unwrap();
        assert_eq!(delivered.at, 1_000);
        assert_eq!(delivered.key.time, 1_000);
        assert_eq!(captured_age(delivered.at, 1_083).unwrap().as_millis(), 99);
        assert!(captured_age(delivered.at, 1_084).is_err());

        let mut mailbox = opened_mailbox();
        let ordinal = mailbox.push(key(0x1e, false, true, 1_000), 1_060).unwrap();
        assert!(mailbox.take(ordinal, 1_084).is_none());
        assert_eq!(mailbox.take_failure(), Some(1));
    }

    #[test]
    fn a_late_hook_callback_never_gets_a_new_posting_timestamp() {
        let mut mailbox = opened_mailbox();
        assert!(mailbox.push(key(0x1e, false, true, 1_000), 1_084).is_none());
        assert_eq!(mailbox.take_failure(), Some(1));
        assert_eq!(mailbox.len, 0);
    }

    #[test]
    fn original_altgr_pairing_survives_different_posting_times() {
        for pressed in [false, true] {
            let mut mailbox = opened_mailbox();
            let ctrl = mailbox
                .push(key(0x1d, false, pressed, 1_000), 1_010)
                .unwrap();
            let alt = mailbox
                .push(key(0x38, true, pressed, 1_000), 1_030)
                .unwrap();
            assert!(mailbox.take(ctrl, 1_040).unwrap().synthetic_control);
            assert!(!mailbox.take(alt, 1_041).unwrap().synthetic_control);
        }
        let mut mailbox = opened_mailbox();
        let ctrl = mailbox.push(key(0x1d, false, true, 1_000), 1_010).unwrap();
        mailbox.push(key(0x38, true, true, 1_001), 1_010).unwrap();
        assert!(!mailbox.take(ctrl, 1_020).unwrap().synthetic_control);
    }

    #[test]
    fn old_notifications_and_delayed_callbacks_cannot_enter_a_new_scope() {
        let mut mailbox = opened_mailbox();
        let old = mailbox.push(key(0x1e, false, true, 1_000), 1_001).unwrap();
        mailbox.deactivate();
        mailbox.activate(Some(2), 1_010);
        assert!(mailbox.push(key(0x1e, false, true, 1_009), 1_011).is_none());
        assert!(mailbox.push(key(0x1e, false, true, 1_010), 1_011).is_none());
        assert!(mailbox.push(key(0x1e, false, true, 900), 1_011).is_none());
        let fresh = mailbox.push(key(0x30, false, true, 1_011), 1_012).unwrap();
        assert_ne!(fresh, old);
        assert!(mailbox.take(old, 1_013).is_none());
        let delivered = mailbox.take(fresh, 1_014).unwrap();
        assert_eq!(delivered.original, 2);
        assert_eq!(delivered.key.scan, 0x30);
        assert!(mailbox.take(fresh, 1_015).is_none());
        assert!(mailbox.take_failure().is_none());
    }

    #[test]
    fn resuming_the_same_session_starts_a_fresh_capture_interval() {
        let mut mailbox = opened_mailbox();
        let old = mailbox.push(key(0x1e, false, true, 1_000), 1_001).unwrap();
        mailbox.deactivate();
        mailbox.activate(Some(1), 1_010);
        assert!(mailbox.take(old, 1_011).is_none());
        assert!(mailbox.push(key(0x1e, false, true, 1_009), 1_011).is_none());
        assert!(mailbox.push(key(0x1e, false, true, 1_011), 1_012).is_some());
    }

    #[test]
    fn presses_and_releases_share_one_fixed_capacity_and_failure_owner() {
        for pressed in [false, true] {
            let mut mailbox = opened_mailbox();
            for _ in 0..MAX_DEFERRED {
                assert!(
                    mailbox
                        .push(key(0x1e, false, pressed, 1_000), 1_001)
                        .is_some()
                );
            }
            assert_eq!(mailbox.len, MAX_DEFERRED);
            assert!(
                mailbox
                    .push(key(0x1e, false, !pressed, 1_000), 1_002)
                    .is_none()
            );
            assert_eq!(mailbox.len, 0);
            mailbox.deactivate();
            mailbox.activate(Some(2), 1_003);
            assert!(mailbox.push(key(0x30, false, true, 1_004), 1_005).is_none());
            for ordinal in 1..=MAX_DEFERRED {
                assert!(mailbox.take(ordinal, 1_005).is_none());
            }
            assert_eq!(mailbox.take_failure(), Some(1));
            mailbox.activate(Some(2), 1_006);
            assert!(mailbox.push(key(0x30, false, true, 1_007), 1_008).is_some());
        }
    }

    #[test]
    fn failed_post_or_missing_predecessor_retires_all_dependent_keys() {
        for failed_post in [false, true] {
            let mut mailbox = opened_mailbox();
            let first = mailbox.push(key(0x1e, false, true, 1_000), 1_001).unwrap();
            let next = mailbox.push(key(0x1e, false, false, 1_002), 1_003).unwrap();
            if failed_post {
                mailbox.post_failed(first);
            } else {
                assert!(mailbox.take(next, 1_004).is_none());
            }
            assert!(mailbox.take(first, 1_005).is_none());
            assert!(mailbox.take(next, 1_005).is_none());
            assert_eq!(mailbox.take_failure(), Some(1));
            assert!(mailbox.take_failure().is_none());
        }
    }

    #[test]
    fn wrapping_native_time_is_extended_once_and_suspend_cannot_alias_it() {
        let wrap = 1_u64 << 32;
        let mut mailbox = Mailbox::new();
        mailbox.activate(Some(1), wrap - 20);
        let ordinal = mailbox
            .push(key(0x1e, false, true, u32::MAX - 9), wrap + 5)
            .unwrap();
        let delivered = mailbox.take(ordinal, wrap + 6).unwrap();
        assert_eq!(delivered.at, wrap - 10);
        assert!(captured_age(delivered.at, 2 * wrap - 10).is_err());
    }

    #[test]
    fn exhausted_notification_ordinals_never_alias_an_old_event() {
        let mut mailbox = opened_mailbox();
        mailbox.next = Some(usize::MAX);
        let ordinal = mailbox.push(key(0x1e, false, true, 1_000), 1_001).unwrap();
        assert_eq!(ordinal, usize::MAX);
        assert!(mailbox.take(ordinal, 1_002).is_some());
        assert!(
            mailbox
                .push(key(0x1e, false, false, 1_003), 1_004)
                .is_none()
        );
        assert_eq!(mailbox.take_failure(), Some(1));
        mailbox.activate(Some(2), 1_005);
        assert!(mailbox.push(key(0x1e, false, true, 1_006), 1_007).is_none());
        assert_eq!(mailbox.take_failure(), Some(2));
    }

    #[test]
    fn retired_keys_still_bound_their_pending_native_notifications() {
        let mut mailbox = opened_mailbox();
        for ordinal in 1..=MAX_DEFERRED {
            let now = 1_000 + u64::try_from(ordinal).unwrap() * 2;
            mailbox.activate(Some(u64::try_from(ordinal).unwrap()), now - 1);
            assert_eq!(
                mailbox.push(key(0x1e, false, true, u32::try_from(now).unwrap()), now),
                Some(ordinal)
            );
            mailbox.deactivate();
        }
        mailbox.activate(Some(100), 2_000);
        assert!(mailbox.push(key(0x1e, false, true, 2_001), 2_001).is_none());
        assert_eq!(mailbox.take_failure(), Some(100));
        for ordinal in 1..=MAX_DEFERRED {
            assert!(mailbox.take(ordinal, 2_002).is_none());
        }
        mailbox.activate(Some(101), 2_003);
        assert!(mailbox.push(key(0x1e, false, true, 2_004), 2_004).is_some());
    }

    #[test]
    fn post_failure_from_retired_scope_does_not_end_the_replacement() {
        let mut mailbox = opened_mailbox();
        let old = mailbox.push(key(0x1e, false, true, 1_000), 1_001).unwrap();
        mailbox.deactivate();
        mailbox.activate(Some(2), 1_002);
        let fresh = mailbox.push(key(0x30, false, true, 1_003), 1_004).unwrap();
        mailbox.post_failed(old);
        assert!(mailbox.take_failure().is_none());
        assert_eq!(mailbox.take(fresh, 1_005).unwrap().original, 2);
        assert!(mailbox.active());
        mailbox.retired();
        assert!(!mailbox.active());
    }

    #[test]
    fn invalid_native_time_fences_the_original_instead_of_losing_a_release() {
        for now in [1_000, (1_u64 << 32) + 1_000] {
            let mut mailbox = Mailbox::new();
            mailbox.activate(Some(7), now - 100);
            assert!(mailbox.push(key(0x1e, false, false, 1_001), now).is_none());
            assert_eq!(mailbox.take_failure(), Some(7));
        }
        let mut mailbox = opened_mailbox();
        assert!(mailbox.push(key(0x1e, false, false, 899), 899).is_none());
        assert_eq!(mailbox.take_failure(), Some(1));
    }
}
