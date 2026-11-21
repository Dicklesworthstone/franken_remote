//! One explicit native file choice: its original dialog budget and accepted
//! gesture age survive native cleanup, descriptor opening and UI delivery.
#![forbid(unsafe_code)]

#[path = "file_choice/selection.rs"]
mod selection;
pub use selection::MAX_SELECTED_FILES;
#[cfg(any(windows, test))]
pub use selection::selected_paths;

pub const CHOICE_MS: u64 = 120_000;
pub const ACCEPTED_MS: u64 = 2_000;
pub const MAX_PATH_UNITS: usize = 32_768;

/// The two native input queues belong to one explicit original dialog. A
/// process ID is insufficient: unrelated windows in this process do not gain
/// an exemption from the ordinary background/control fence.
#[derive(Clone, Copy)]
pub struct Focus {
    owner_thread: u32,
    owner_window: usize,
    lifetime: u64,
    worker_thread: Option<u32>,
    dialog_window: Option<usize>,
    published: bool,
    worker_live: bool,
}

impl Focus {
    pub fn new(owner_thread: u32, owner_window: usize, lifetime: u64) -> Option<Self> {
        (owner_thread != 0 && owner_window != 0 && lifetime != u64::MAX).then_some(Self {
            owner_thread,
            owner_window,
            lifetime,
            worker_thread: None,
            dialog_window: None,
            published: false,
            worker_live: false,
        })
    }

    pub fn publish(&mut self) -> bool {
        if self.published {
            return false;
        }
        self.published = true;
        true
    }

    pub fn register_worker(&mut self, thread: u32) -> bool {
        if !self.published
            || thread == 0
            || thread == self.owner_thread
            || self.worker_thread.is_some()
        {
            return false;
        }
        self.worker_thread = Some(thread);
        self.worker_live = true;
        true
    }

    pub fn register_dialog(&mut self, thread: u32, window: usize) -> bool {
        if !self.allows_picker(thread, self.lifetime)
            || window == 0
            || window == self.owner_window
            || self.dialog_window.is_some()
        {
            return false;
        }
        self.dialog_window = Some(window);
        true
    }

    pub fn allows_picker(&self, thread: u32, lifetime: u64) -> bool {
        self.published
            && self.worker_live
            && self.lifetime == lifetime
            && self.worker_thread == Some(thread)
    }

    pub fn allows_owner(&self, thread: u32) -> bool {
        self.worker_live && thread != 0 && thread == self.owner_thread
    }

    pub fn current(&self, lifetime: u64) -> bool {
        self.published && self.lifetime == lifetime
    }

    pub fn owner_foreground(&self, thread: u32, root: usize) -> bool {
        thread == self.owner_thread && root == self.owner_window
    }

    pub fn foreground(&self, thread: u32, root: usize) -> bool {
        self.owner_foreground(thread, root)
            || (self.worker_live
                && self.worker_thread == Some(thread)
                && self.dialog_window == Some(root))
    }

    /// Retire before the native thread terminates; a retained join handle does
    /// not prevent Windows from reusing its numeric thread ID.
    pub fn finish_worker(&mut self) {
        self.worker_live = false;
    }
}

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
    fn a_worker_cannot_receive_focus_before_its_original_choice_is_published() {
        let mut focus = Focus::new(10, 100, 5).unwrap();
        assert!(!focus.register_worker(20));
        assert!(!focus.allows_picker(20, 5));
        assert!(focus.publish());
        assert!(!focus.publish());
        assert!(!focus.register_worker(0));
        assert!(!focus.register_worker(10));
        assert!(focus.register_worker(20));
        assert!(focus.allows_picker(20, 5));
        assert!(!focus.register_worker(21));
        for (thread, lifetime) in [(0, 5), (21, 5), (20, 6)] {
            assert!(!focus.allows_picker(thread, lifetime));
        }
    }

    #[test]
    fn only_original_viewer_and_dialog_roots_can_retain_choice_focus() {
        let mut focus = Focus::new(10, 100, 5).unwrap();
        focus.publish();
        focus.register_worker(20);
        assert!(!focus.register_dialog(21, 200));
        assert!(!focus.register_dialog(20, 0));
        assert!(focus.register_dialog(20, 200));
        assert!(!focus.register_dialog(20, 201));
        assert!(focus.allows_owner(10));
        assert!(focus.foreground(10, 100));
        assert!(focus.foreground(20, 200));
        for (thread, window) in [(0, 100), (10, 101), (20, 201), (21, 200), (20, 100)] {
            assert!(!focus.foreground(thread, window));
        }
        assert!(!focus.allows_owner(0));
        assert!(!focus.allows_owner(21));
        assert!(!focus.owner_foreground(20, 200));
    }

    #[test]
    fn an_exited_worker_id_cannot_be_reused_as_an_activation_exemption() {
        let mut focus = Focus::new(10, 100, 5).unwrap();
        focus.publish();
        focus.register_worker(20);
        focus.register_dialog(20, 200);
        focus.finish_worker();
        assert!(!focus.allows_picker(20, 5));
        assert!(!focus.allows_owner(10));
        assert!(!focus.foreground(20, 200));
        assert!(!focus.register_worker(20));
        assert!(focus.owner_foreground(10, 100));
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
