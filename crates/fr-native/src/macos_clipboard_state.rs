//! Bounded pasteboard ownership and publication policy, independent of `AppKit`.
#![forbid(unsafe_code)]

use fr_core::clipboard::{PlatformError, Publication, Stamp};

#[derive(Clone, Copy)]
pub(crate) struct Change {
    pub(crate) revision: u64,
    pub(crate) count: i64,
    pub(crate) has_text: bool,
    pub(crate) origin: Option<Stamp>,
}

/// `changeCount` can start at zero. Wire revisions are positive, monotonic and
/// local to this original native owner; a pasteboard-server reset is a refusal.
#[derive(Default)]
pub(crate) struct Revisions {
    current: Option<Change>,
    pending: bool,
}

impl Revisions {
    pub(crate) fn current(&self) -> Option<Change> {
        self.current
    }

    pub(crate) fn revision(&self) -> u64 {
        self.current().map_or(0, |change| change.revision)
    }

    pub(crate) fn observe(&mut self, count: i64, has_text: bool) -> Result<Change, PlatformError> {
        if count < 0 || self.current.is_some_and(|old| count < old.count) {
            return Err(PlatformError::Unavailable);
        }
        if let Some(old) = self
            .current
            .filter(|old| old.count == count && old.has_text == has_text)
        {
            return Ok(old);
        }
        let revision = self
            .revision()
            .checked_add(1)
            .ok_or(PlatformError::Unavailable)?;
        let change = Change {
            revision,
            count,
            has_text,
            // Only a confirmed publication may install provenance. Identical
            // text, an external custom pasteboard type, or a count gap cannot.
            origin: None,
        };
        self.current = Some(change);
        self.pending = true;
        Ok(change)
    }

    pub(crate) fn bootstrap(&mut self) {
        self.pending = self.current.is_some();
    }

    pub(crate) fn take_pending(&mut self) -> Option<Change> {
        if std::mem::take(&mut self.pending) {
            self.current
        } else {
            None
        }
    }

    pub(crate) fn published(&mut self, count: i64, stamp: Stamp) -> Result<(), PlatformError> {
        if !stamp.valid() {
            return Err(PlatformError::Unsupported);
        }
        let mut change = self.observe(count, true)?;
        change.origin = Some(stamp);
        self.current = Some(change);
        self.pending = true;
        Ok(())
    }

    /// Keep the monotonic floor across a subscription pause, but forget private
    /// provenance. The synchronizer owns the re-enable bootstrap/no-replay rule.
    pub(crate) fn suspend(&mut self) {
        if let Some(change) = self.current.as_mut() {
            change.origin = None;
        }
        self.pending = false;
    }
}

/// `NSPasteboard` has no compare-and-swap replacement. Its declaration both
/// clears and increments ownership; detect a copy racing the preflight count
/// and never follow that discontinuity with another write or a restoration.
pub(crate) struct Replacement {
    next: i64,
}

impl Replacement {
    pub(crate) fn new(before: i64) -> Result<Self, PlatformError> {
        Ok(Self {
            next: before
                .checked_add(1)
                .filter(|next| *next > 0)
                .ok_or(PlatformError::Unavailable)?,
        })
    }

    pub(crate) fn declared(&self, count: i64) -> bool {
        count == self.next
    }

    pub(crate) fn finish(&self, submitted: bool, count: i64, allowed: bool) -> Publication {
        if submitted && self.declared(count) && allowed {
            Publication::SubmittedToOs
        } else {
            Publication::UnknownEffect
        }
    }
}

pub(crate) const fn refused(attempted: bool, error: PlatformError) -> Publication {
    if attempted {
        Publication::UnknownEffect
    } else {
        Publication::NotSubmitted(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::clipboard::Endpoint;

    fn stamp() -> Stamp {
        Stamp {
            id: 9,
            source: Endpoint::Host,
            sequence: 1,
        }
    }

    #[test]
    fn zero_native_count_and_empty_text_are_valid_selected_revision() {
        let mut state = Revisions::default();
        let change = state.observe(0, true).unwrap();
        assert_eq!(change.revision, 1);
        assert!(change.has_text);
        assert!(state.take_pending().is_some());
        assert!(state.take_pending().is_none());
        state.observe(0, true).unwrap();
        assert!(state.take_pending().is_none());
    }

    #[test]
    fn only_exact_publication_ownership_suppresses_echo() {
        let mut state = Revisions::default();
        state.observe(2, true).unwrap();
        state.published(3, stamp()).unwrap();
        assert_eq!(state.take_pending().unwrap().origin, Some(stamp()));
        assert_eq!(state.observe(3, true).unwrap().origin, Some(stamp()));
        // The bytes may be identical: a genuine new copy still propagates.
        let copied = state.observe(4, true).unwrap();
        assert_eq!(copied.revision, 3);
        assert_eq!(copied.origin, None);
        assert_eq!(state.take_pending().unwrap().origin, None);
    }

    #[test]
    fn latest_change_is_bounded_and_metadata_does_not_invent_provenance() {
        let mut state = Revisions::default();
        for count in 0..1000 {
            state.observe(count, count % 2 == 0).unwrap();
        }
        let latest = state.take_pending().unwrap();
        assert_eq!(latest.count, 999);
        assert_eq!(latest.revision, 1000);
        assert!(!latest.has_text);
        assert_eq!(latest.origin, None);
        assert!(state.take_pending().is_none());
    }

    #[test]
    fn suspend_keeps_floor_and_new_watch_does_not_manufacture_copy() {
        let mut state = Revisions::default();
        state.published(7, stamp()).unwrap();
        state.suspend();
        let before = state.revision();
        state.observe(7, true).unwrap();
        state.bootstrap();
        let bootstrap = state.take_pending().unwrap();
        assert_eq!(bootstrap.revision, before);
        assert_eq!(bootstrap.origin, None);
        assert!(state.observe(8, true).unwrap().revision > before);
    }

    #[test]
    fn server_reset_regression_and_revision_exhaustion_refuse() {
        let mut state = Revisions::default();
        assert!(state.observe(-1, false).is_err());
        state.observe(9, true).unwrap();
        assert!(state.observe(0, true).is_err());
        state.current.as_mut().unwrap().revision = u64::MAX;
        assert!(state.observe(10, true).is_err());
        assert_eq!(state.current().unwrap().count, 9);
    }

    #[test]
    fn declaration_race_or_post_mutation_failure_never_claims_no_effect() {
        let replacement = Replacement::new(7).unwrap();
        assert!(replacement.declared(8));
        assert!(!replacement.declared(9));
        assert!(!replacement.declared(7));
        assert!(Replacement::new(i64::MAX).is_err());
        assert_eq!(
            replacement.finish(true, 8, true),
            Publication::SubmittedToOs
        );
        for (submitted, count, allowed) in [(false, 8, true), (true, 9, true), (true, 8, false)] {
            assert_eq!(
                replacement.finish(submitted, count, allowed),
                Publication::UnknownEffect
            );
        }
        assert_eq!(
            refused(true, PlatformError::Permission),
            Publication::UnknownEffect
        );
        assert_eq!(
            refused(false, PlatformError::LocalChanged),
            Publication::NotSubmitted(PlatformError::LocalChanged)
        );
    }
}
