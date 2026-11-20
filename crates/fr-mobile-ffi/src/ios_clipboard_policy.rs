//! `UIKit`'s change count advances at the end of a run-loop turn. Keep an
//! original publication's provenance across that delayed notification without
//! comparing clipboard contents or interpreting a timer as a native change.
#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PRESENCE: Duration = Duration::from_millis(300);

/// The persistent native clipboard control must remain visible and serviced.
/// Once its presence lapses, a later callback cannot turn sharing back on.
#[derive(Clone, Default)]
pub(crate) struct Privacy(Arc<Mutex<Presence>>);

#[derive(Default)]
struct Presence {
    last: Option<Instant>,
    stopped: bool,
}

impl Privacy {
    pub(crate) fn observe(&self, visible: bool) {
        self.observe_at(visible, Instant::now());
    }

    fn observe_at(&self, visible: bool, now: Instant) {
        let Ok(mut state) = self.0.lock() else { return };
        if (!visible && state.last.is_some())
            || state
                .last
                .is_some_and(|last| now < last || now.duration_since(last) > PRESENCE)
        {
            state.stopped = true;
        }
        if visible && !state.stopped {
            state.last = Some(now);
        }
    }

    pub(crate) fn stop(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.stopped = true;
        }
    }

    pub(crate) fn live(&self) -> bool {
        self.live_at(Instant::now())
    }

    pub(crate) fn stopped(&self) -> bool {
        self.0.lock().map_or(true, |state| state.stopped)
    }

    fn live_at(&self, now: Instant) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        let live = state
            .last
            .is_some_and(|last| now >= last && now.duration_since(last) <= PRESENCE);
        if state.last.is_some() && !live {
            state.stopped = true;
        }
        live && !state.stopped
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Metadata {
    pub(crate) count: u64,
    pub(crate) text: bool,
    /// The exact private type written by THIS owner is still present. This is
    /// native representation metadata, never a guess from text equality.
    pub(crate) own: bool,
}

#[derive(Default)]
pub(crate) struct Revisions {
    current: Option<Metadata>,
    revision: u32,
    failed: bool,
}

impl Revisions {
    pub(crate) fn current(&self) -> Option<Metadata> {
        (!self.failed).then_some(self.current).flatten()
    }

    pub(crate) fn revision(&self) -> u32 {
        self.revision
    }

    pub(crate) fn observe(&mut self, next: Metadata) -> Result<u32, ()> {
        if self.failed || self.current.is_some_and(|old| next.count < old.count) {
            self.failed = true;
            return Err(());
        }
        let changed = self.current.is_none_or(|old| {
            old.text != next.text
                || old.own != next.own
                || (old.count != next.count && !(old.own && next.own))
        });
        if changed {
            self.advance()?;
        }
        self.current = Some(next);
        Ok(self.revision)
    }

    /// A completed `setItems` call must leave its exact new marker and a text
    /// representation. Its revision advances even when `UIKit`'s raw count has
    /// not advanced yet; the later notification cannot masquerade as a copy.
    pub(crate) fn published(&mut self, next: Metadata) -> Result<u32, ()> {
        if self.failed
            || !next.own
            || !next.text
            || self.current.is_some_and(|old| next.count < old.count)
        {
            self.failed = true;
            return Err(());
        }
        self.advance()?;
        self.current = Some(next);
        Ok(self.revision)
    }

    fn advance(&mut self) -> Result<(), ()> {
        let Some(next) = self.revision.checked_add(1) else {
            self.failed = true;
            return Err(());
        };
        self.revision = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(count: u64, text: bool, own: bool) -> Metadata {
        Metadata { count, text, own }
    }

    #[test]
    fn delayed_native_count_never_echoes_the_original_publication() {
        let mut revisions = Revisions::default();
        assert_eq!(revisions.observe(metadata(9, true, false)), Ok(1));
        assert_eq!(revisions.published(metadata(9, true, true)), Ok(2));
        assert_eq!(revisions.observe(metadata(9, true, true)), Ok(2));
        assert_eq!(revisions.observe(metadata(10, true, true)), Ok(2));
        assert_eq!(revisions.current(), Some(metadata(10, true, true)));
        assert_eq!(revisions.observe(metadata(11, true, false)), Ok(3));
    }

    #[test]
    fn different_local_copies_and_repeated_remote_publications_have_new_revisions() {
        let mut revisions = Revisions::default();
        assert_eq!(revisions.observe(metadata(3, false, false)), Ok(1));
        assert_eq!(revisions.observe(metadata(4, true, false)), Ok(2));
        assert_eq!(revisions.observe(metadata(5, true, false)), Ok(3));
        assert_eq!(revisions.published(metadata(5, true, true)), Ok(4));
        assert_eq!(revisions.published(metadata(5, true, true)), Ok(5));
        assert_eq!(revisions.observe(metadata(7, true, true)), Ok(5));
        assert_eq!(revisions.revision(), 5);
        assert_eq!(revisions.observe(metadata(7, false, false)), Ok(6));
    }

    #[test]
    fn missing_provenance_counter_regression_and_exhaustion_are_terminal() {
        for next in [metadata(1, true, false), metadata(1, false, true)] {
            let mut revisions = Revisions::default();
            revisions.observe(metadata(1, false, false)).unwrap();
            assert_eq!(revisions.published(next), Err(()));
            assert_eq!(revisions.observe(metadata(2, true, true)), Err(()));
            assert_eq!(revisions.current(), None);
        }
        let mut revisions = Revisions::default();
        revisions.observe(metadata(9, true, false)).unwrap();
        assert_eq!(revisions.observe(metadata(8, true, false)), Err(()));
        let mut revisions = Revisions {
            revision: u32::MAX,
            ..Revisions::default()
        };
        assert_eq!(revisions.observe(metadata(1, true, false)), Err(()));
    }

    #[test]
    fn presence_requires_a_real_visible_callback_and_never_restarts_after_expiry() {
        let privacy = Privacy::default();
        let start = Instant::now();
        assert!(!privacy.live_at(start));
        privacy.observe_at(true, start);
        assert!(privacy.live_at(start + PRESENCE));
        assert!(!privacy.live_at(start + PRESENCE + Duration::from_nanos(1)));
        privacy.observe_at(true, start + PRESENCE + Duration::from_millis(1));
        assert!(!privacy.live_at(start + PRESENCE + Duration::from_millis(1)));
    }

    #[test]
    fn pending_hidden_surface_cannot_access_clipboard_but_can_become_visible_once() {
        let privacy = Privacy::default();
        let start = Instant::now();
        privacy.observe(false);
        assert!(!privacy.live());
        privacy.observe_at(false, start);
        assert!(!privacy.live_at(start));
        assert!(!privacy.stopped());
        privacy.observe_at(true, start);
        assert!(privacy.live_at(start));
        privacy.observe_at(false, start + Duration::from_millis(1));
        privacy.observe_at(true, start + Duration::from_millis(2));
        assert!(!privacy.live_at(start + Duration::from_millis(2)));
    }

    #[test]
    fn native_guard_and_ui_clone_share_one_terminal_stop() {
        let privacy = Privacy::default();
        let worker = privacy.clone();
        let start = Instant::now();
        privacy.observe_at(true, start);
        assert!(worker.live_at(start));
        worker.stop();
        privacy.observe_at(true, start);
        assert!(privacy.stopped());
        assert!(!worker.live_at(start));
    }

    #[test]
    fn a_regressed_presence_clock_cannot_resume_the_original_owner() {
        let privacy = Privacy::default();
        let start = Instant::now();
        privacy.observe_at(true, start + Duration::from_millis(1));
        privacy.observe_at(true, start);
        assert!(privacy.stopped());
        assert!(!privacy.live_at(start + Duration::from_millis(1)));
    }

    #[test]
    fn a_stalled_ui_cannot_retime_its_presence_and_off_is_terminal() {
        let start = Instant::now();
        for stop in [true, false] {
            let privacy = Privacy::default();
            privacy.observe_at(true, start);
            if stop {
                privacy.stop();
            } else {
                privacy.observe_at(false, start);
            }
            privacy.observe_at(true, start);
            assert!(!privacy.live_at(start));
        }
        let privacy = Privacy::default();
        privacy.observe_at(true, start);
        privacy.observe_at(true, start + PRESENCE + Duration::from_millis(1));
        assert!(!privacy.live_at(start + PRESENCE + Duration::from_millis(1)));
    }
}
