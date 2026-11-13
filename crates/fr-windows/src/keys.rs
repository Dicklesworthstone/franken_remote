//! Windows keyboard messages for the viewer: the synthetic left Ctrl around
//! `AltGr`, and the bounded queue of key transitions the session has not
//! taken yet. The scan-code table (set 1 to USB HID usages) is
//! `fr_core::input::windows_scan`, shared with frd's Windows host, which maps
//! the usages back. Platform-neutral so every CI host tests it.

/// One keyboard message as the window procedure sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyMessage {
    pub scan: u8,
    pub extended: bool,
    pub pressed: bool,
    /// The message time (milliseconds, `GetMessageTime`).
    pub time: u32,
}

/// Whether `this` is the left Ctrl Windows synthesizes around `AltGr` on
/// layouts that have it: a left Ctrl transition (scan 0x1D, not extended)
/// queued directly ahead of a right Alt transition (scan 0x38, extended) in
/// the same direction with the same message time. The host types with its
/// own layout, where right Alt is `AltGr`; forwarding the synthetic Ctrl
/// would make every `AltGr` character Ctrl+`AltGr` (`@` on a German layout
/// arrives as Ctrl+@). A real left Ctrl is a separate keystroke with its own
/// time.
pub fn synthetic_ctrl(this: KeyMessage, next: Option<KeyMessage>) -> bool {
    this.scan == 0x1D
        && !this.extended
        && next.is_some_and(|next| {
            next.scan == 0x38
                && next.extended
                && next.pressed == this.pressed
                && next.time == this.time
        })
}

/// Key transitions the session has not taken yet. Each payload carries its
/// original source timestamp and deadline; retrying only borrows that payload.
/// The native message time also survives every retry, including a host suspend.
/// No key content is printable through this queue.
pub struct Deferred<T> {
    queue: std::collections::VecDeque<Captured<T>>,
    failure: Option<QueueError>,
    last_now: Option<u64>,
}

struct Captured<T> {
    event: T,
    /// Original Windows message time, extended to 64-bit system uptime.
    at: u64,
}

impl<T> Default for Deferred<T> {
    fn default() -> Self {
        Self {
            queue: std::collections::VecDeque::new(),
            failure: None,
            last_now: None,
        }
    }
}

/// All transitions count, including releases. A lost release cannot leave a
/// live remote hold: an overflow fences the original source and its cleanup.
pub const MAX_DEFERRED: usize = 64;
/// The same 100 ms freshness budget as frd's captured-event source.
pub const MAX_AGE_MS: u64 = 100;
/// Win32 message/tick time has coarse system-timer resolution. Reserve one
/// 16 ms tick rather than giving a rounded native timestamp a fresh lifetime.
pub const TICK_MARGIN_MS: u64 = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    Expired,
    Overflow,
    Clock,
    Refused,
}

/// Extend a Win32 message's wrapping uptime into the current 64-bit tick
/// domain. Future and old values are refused, never re-stamped at receipt.
pub fn message_time(message: u32, now: u64) -> Result<u64, QueueError> {
    let low = u32::try_from(now & u64::from(u32::MAX)).map_err(|_| QueueError::Clock)?;
    let age = u64::from(low.wrapping_sub(message));
    if age.saturating_add(TICK_MARGIN_MS) >= MAX_AGE_MS {
        return Err(QueueError::Expired);
    }
    now.checked_sub(age).ok_or(QueueError::Clock)
}

impl<T> Deferred<T> {
    /// Queue a new original event, then send in capture order. A busy source
    /// leaves exactly that event at the front. Failure is terminal, clears
    /// every unsent event, and must fence control before any other input.
    pub fn push(
        &mut self,
        event: T,
        at: u64,
        mut now: impl FnMut() -> u64,
        mut send: impl FnMut(&T) -> Result<bool, ()>,
    ) -> Result<(), QueueError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        let current = now();
        self.check(at, current)?;
        // Drain before testing capacity: a release can use a slot that the
        // original source just made available, but never bypass the bound.
        self.drain(&mut now, &mut send)?;
        if self.queue.len() >= MAX_DEFERRED {
            return self.fail(QueueError::Overflow);
        }
        self.queue.push_back(Captured { event, at });
        self.drain(now, send)
    }

    /// The native clock is sampled for EACH event, so a slow send cannot lend
    /// its earlier sample to the next key. `send` also retains the immutable
    /// client-clock bound through frd's source and transport queues.
    pub fn drain(
        &mut self,
        mut now: impl FnMut() -> u64,
        mut send: impl FnMut(&T) -> Result<bool, ()>,
    ) -> Result<(), QueueError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        while let Some(first) = self.queue.front() {
            let at = first.at;
            self.check(at, now())?;
            let Some(first) = self.queue.front() else {
                return self.fail(QueueError::Refused);
            };
            match send(&first.event) {
                Ok(true) => {
                    self.queue.pop_front();
                }
                Ok(false) => return Ok(()),
                Err(()) => return self.fail(QueueError::Refused),
            }
        }
        Ok(())
    }

    fn check(&mut self, at: u64, now: u64) -> Result<(), QueueError> {
        if self.last_now.is_some_and(|last| now < last) || at > now {
            return self.fail(QueueError::Clock);
        }
        self.last_now = Some(now);
        if now.saturating_sub(at).saturating_add(TICK_MARGIN_MS) >= MAX_AGE_MS {
            return self.fail(QueueError::Expired);
        }
        Ok(())
    }

    fn fail(&mut self, error: QueueError) -> Result<(), QueueError> {
        self.queue.clear();
        self.failure = Some(error);
        Err(error)
    }

    /// Forget unsent events on focus loss or closure. This cannot revive a
    /// failed queue; only a new session may construct another queue.
    pub fn clear(&mut self) {
        self.queue.clear();
    }
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_ctrl_windows_synthesizes_for_altgr_is_dropped() {
        let message = |scan, extended, pressed, time| KeyMessage {
            scan,
            extended,
            pressed,
            time,
        };
        let ctrl = message(0x1D, false, true, 1000);
        let altgr = message(0x38, true, true, 1000);
        assert!(synthetic_ctrl(ctrl, Some(altgr)));
        // The release pair, too.
        assert!(synthetic_ctrl(
            message(0x1D, false, false, 2000),
            Some(message(0x38, true, false, 2000))
        ));
        // Planted negatives: a real Ctrl (its own time), right Ctrl, left
        // Alt, opposite directions, nothing queued, and AltGr itself.
        assert!(!synthetic_ctrl(ctrl, Some(message(0x38, true, true, 1001))));
        assert!(!synthetic_ctrl(
            message(0x1D, true, true, 1000),
            Some(altgr)
        ));
        assert!(!synthetic_ctrl(
            ctrl,
            Some(message(0x38, false, true, 1000))
        ));
        assert!(!synthetic_ctrl(
            ctrl,
            Some(message(0x38, true, false, 1000))
        ));
        assert!(!synthetic_ctrl(ctrl, None));
        assert!(!synthetic_ctrl(altgr, Some(altgr)));
    }

    #[test]
    fn a_busy_press_keeps_the_original_events_and_deadlines_in_order() {
        let mut deferred = Deferred::default();
        let mut sent = Vec::new();
        let events = [
            (0xE1, true, 1_001),
            (0x04, true, 1_002),
            (0x04, false, 1_003),
            (0xE1, false, 1_004),
        ];
        for event in events {
            deferred
                .push(event, event.2, || 1_010, |_| Ok(false))
                .unwrap();
        }
        deferred.drain(|| 1_020, |_| Ok(false)).unwrap();
        assert!(!deferred.is_empty());
        deferred
            .drain(
                || 1_030,
                |event| {
                    sent.push(*event);
                    Ok(true)
                },
            )
            .unwrap();
        assert_eq!(sent, events);
        assert!(deferred.is_empty());
    }

    #[test]
    fn both_press_and_release_floods_obey_one_terminal_bound() {
        for pressed in [false, true] {
            let mut deferred = Deferred::default();
            for index in 0..MAX_DEFERRED {
                deferred
                    .push((index, pressed), 1_000, || 1_001, |_| Ok(false))
                    .unwrap();
            }
            assert_eq!(deferred.queue.len(), MAX_DEFERRED);
            assert_eq!(
                deferred.push((0, !pressed), 1_000, || 1_001, |_| Ok(false)),
                Err(QueueError::Overflow),
            );
            assert!(deferred.is_empty());
            deferred.clear();
            let mut called = false;
            assert_eq!(
                deferred.drain(
                    || 1_002,
                    |_| {
                        called = true;
                        Ok(true)
                    }
                ),
                Err(QueueError::Overflow)
            );
            assert!(
                !called,
                "neither cleanup nor capacity can revive a failed source"
            );
        }
    }

    #[test]
    fn exact_native_deadline_and_suspend_refuse_without_replaying() {
        for now in [1_084, 50_000, (1_u64 << 32) + 1_000] {
            let mut deferred = Deferred::default();
            deferred
                .push((0xE1, true), 1_000, || 1_000, |_| Ok(false))
                .unwrap();
            deferred
                .push((0x04, true), 1_001, || 1_001, |_| Ok(false))
                .unwrap();
            let mut sent = Vec::new();
            assert_eq!(
                deferred.drain(
                    || now,
                    |event| {
                        sent.push(*event);
                        Ok(true)
                    }
                ),
                Err(QueueError::Expired)
            );
            assert_eq!(sent, []);
            assert!(deferred.is_empty());
        }
    }

    #[test]
    fn sending_one_key_cannot_renew_the_next_keys_native_bound() {
        let mut deferred = Deferred::default();
        for event in 0..3 {
            deferred
                .push(event, 1_000, || 1_000, |_| Ok(false))
                .unwrap();
        }
        let now = std::cell::Cell::new(1_083);
        let mut sent = Vec::new();
        let result = deferred.drain(
            || now.get(),
            |event| {
                sent.push(*event);
                now.set(1_084); // first enqueue blocked until the next key expired
                Ok(true)
            },
        );
        assert_eq!(result, Err(QueueError::Expired));
        assert_eq!(sent, [0]);
        assert!(deferred.is_empty());
    }

    #[test]
    fn a_refused_source_discards_followers_and_never_retries_an_uncertain_key() {
        let mut deferred = Deferred::default();
        for event in 0..3 {
            deferred
                .push(event, 1_000, || 1_000, |_| Ok(false))
                .unwrap();
        }
        let mut attempted = Vec::new();
        assert_eq!(
            deferred.drain(
                || 1_001,
                |event| {
                    attempted.push(*event);
                    Err(())
                }
            ),
            Err(QueueError::Refused)
        );
        assert_eq!(attempted, [0]);
        assert!(deferred.is_empty());
        assert_eq!(
            deferred.push(4, 1_002, || 1_002, |_| panic!("must not replay")),
            Err(QueueError::Refused)
        );
    }

    #[test]
    fn focus_loss_discards_waiting_transitions_and_clock_regression_is_terminal() {
        let mut deferred = Deferred::default();
        deferred.push(4, 1_000, || 1_001, |_| Ok(false)).unwrap();
        deferred.clear();
        deferred
            .drain(|| 1_002, |_| panic!("focus loss dropped the key"))
            .unwrap();
        assert_eq!(
            deferred.push(5, 999, || 999, |_| Ok(true)),
            Err(QueueError::Clock)
        );
    }

    #[test]
    fn native_message_time_handles_wrap_and_refuses_old_or_future_values() {
        let wrap = 1_u64 << 32;
        assert_eq!(message_time(u32::MAX - 9, wrap + 5), Ok(wrap - 10));
        assert_eq!(message_time(1_000, 1_083), Ok(1_000));
        assert_eq!(message_time(1_000, 1_084), Err(QueueError::Expired));
        assert_eq!(message_time(1_001, 1_000), Err(QueueError::Expired));
        assert_eq!(message_time(u32::MAX, 0), Err(QueueError::Clock));
    }
}
