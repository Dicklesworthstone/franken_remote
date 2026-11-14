//! One local machine choice from one immutable installed-daemon snapshot.
//!
//! No address, session or input authority lives here. The selected ID/name
//! must still pass the connector's fresh authenticated lookup.
#![forbid(unsafe_code)]

use crate::keys::message_time;
use fr_client::discovery::picker::{Candidate, Catalog};

/// Discovery, window construction, choice and cleanup share this original
/// lifetime. Showing a window or receiving a click never starts a new budget.
const CHOICE_MILLIS: u64 = 120_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Error {
    InvalidChoice,
    StaleEvent,
    Cancelled,
    Expired,
    ClockChanged,
    EventFlood,
    NativeUnavailable,
    CleanupIncomplete,
    AlreadyUsed,
}

#[derive(Clone, Copy)]
enum Decision {
    Waiting,
    Chosen(usize),
    Refused(Error),
    Consumed,
}

pub(crate) struct Choice {
    catalog: Catalog,
    decision: Decision,
    deadline: u64,
    last_now: u64,
    shown: Option<u64>,
}

impl Choice {
    pub(crate) fn new(catalog: Catalog, started: u64) -> Result<Self, Error> {
        // Empty catalogs remain visible, but can never produce a selection.
        Ok(Self {
            catalog,
            decision: Decision::Waiting,
            deadline: started
                .checked_add(CHOICE_MILLIS)
                .ok_or(Error::ClockChanged)?,
            last_now: started,
            shown: None,
        })
    }

    pub(crate) fn candidates(&self) -> &[Candidate] {
        self.catalog.candidates()
    }

    /// Only events after the completed native window setup can choose a row.
    /// A second show cannot renew the snapshot or move its original boundary.
    pub(crate) fn shown(&mut self, now: u64) {
        if !self.complete(now) && self.shown.is_none() {
            self.shown = Some(now);
        }
    }

    pub(crate) fn choose(&mut self, row: usize, message: u32, now: u64) {
        if self.complete(now) {
            return;
        }
        let recent = message_time(message, now)
            .ok()
            .zip(self.shown)
            .is_some_and(|(event, shown)| event > shown);
        self.decision = if !recent {
            Decision::Refused(Error::StaleEvent)
        } else if self.catalog.get(row).is_err() {
            Decision::Refused(Error::InvalidChoice)
        } else {
            Decision::Chosen(row)
        };
    }

    /// A chosen row is provisional until the queued native batch and cleanup
    /// complete. Cancellation and native failures can still invalidate it.
    pub(crate) fn refuse(&mut self, error: Error) {
        if matches!(self.decision, Decision::Waiting | Decision::Chosen(_)) {
            self.decision = Decision::Refused(error);
        }
    }

    pub(crate) fn complete(&mut self, now: u64) -> bool {
        if now < self.last_now {
            self.refuse(Error::ClockChanged);
        } else if now >= self.deadline {
            self.refuse(Error::Expired);
        }
        self.last_now = now;
        !matches!(self.decision, Decision::Waiting)
    }

    /// The native owner calls this only after confirmed window destruction.
    /// Recheck the original deadline after cleanup, then consume exactly once.
    pub(crate) fn take(&mut self, now: u64) -> Result<Candidate, Error> {
        self.complete(now);
        match std::mem::replace(&mut self.decision, Decision::Consumed) {
            Decision::Chosen(row) => self
                .catalog
                .get(row)
                .cloned()
                .map_err(|_| Error::InvalidChoice),
            Decision::Refused(error) => Err(error),
            Decision::Waiting => Err(Error::InvalidChoice),
            Decision::Consumed => Err(Error::AlreadyUsed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_client::discovery::picker::PathHint;

    fn catalog() -> Catalog {
        Catalog::new(vec![
            Candidate::new(
                "n-z".to_owned(),
                "z.tail.ts.net".to_owned(),
                PathHint::Unknown,
            )
            .unwrap(),
            Candidate::new(
                "n-a".to_owned(),
                "a.tail.ts.net".to_owned(),
                PathHint::Direct,
            )
            .unwrap(),
        ])
        .unwrap()
    }

    fn shown() -> Choice {
        let mut choice = Choice::new(catalog(), 1_000).unwrap();
        choice.shown(1_010);
        choice
    }

    #[test]
    fn exact_original_row_is_consumed_once() {
        let mut choice = shown();
        assert_eq!(choice.candidates()[0].stable_id(), "n-a");
        choice.choose(0, 1_020, 1_021);
        assert!(choice.complete(1_022));
        let peer = choice.take(1_023).unwrap();
        assert_eq!(peer.stable_id(), "n-a");
        assert_eq!(peer.certificate_name(), "a.tail.ts.net");
        assert_eq!(peer.path_hint(), PathHint::Direct);
        assert_eq!(choice.take(1_024), Err(Error::AlreadyUsed));
    }

    #[test]
    fn empty_and_invalid_rows_never_fall_back_or_recover() {
        let mut empty = Choice::new(Catalog::new(Vec::new()).unwrap(), 1_000).unwrap();
        assert_eq!(empty.candidates(), []);
        empty.shown(1_010);
        empty.choose(0, 1_020, 1_021);
        assert_eq!(empty.take(1_022), Err(Error::InvalidChoice));
        for row in [2, usize::MAX] {
            let mut choice = shown();
            choice.choose(row, 1_020, 1_021);
            choice.choose(0, 1_022, 1_023);
            assert_eq!(choice.take(1_024), Err(Error::InvalidChoice));
        }
    }

    #[test]
    fn cancellation_and_native_faults_override_a_pending_choice() {
        for error in [
            Error::Cancelled,
            Error::EventFlood,
            Error::NativeUnavailable,
            Error::CleanupIncomplete,
        ] {
            let mut choice = shown();
            choice.choose(0, 1_020, 1_021);
            choice.refuse(error);
            choice.choose(1, 1_022, 1_023);
            assert_eq!(choice.take(1_024), Err(error));
        }
    }

    #[test]
    fn queued_before_show_same_tick_future_and_stale_events_are_terminal() {
        for (message, now) in [(999, 1_020), (1_010, 1_020), (1_021, 1_020), (1_020, 1_104)] {
            let mut choice = shown();
            choice.choose(0, message, now);
            choice.choose(1, 1_200, 1_201);
            assert_eq!(choice.take(1_202), Err(Error::StaleEvent));
        }
        let mut before_show = Choice::new(catalog(), 1_000).unwrap();
        before_show.choose(0, 1_010, 1_011);
        before_show.shown(1_012);
        assert_eq!(before_show.take(1_013), Err(Error::StaleEvent));
    }

    #[test]
    fn original_deadline_includes_discovery_setup_and_cleanup() {
        let mut cleanup = Choice::new(catalog(), 1_000).unwrap();
        cleanup.shown(120_990);
        cleanup.choose(0, 120_995, 120_996);
        assert_eq!(cleanup.take(121_000), Err(Error::Expired));

        let mut setup = Choice::new(catalog(), 1_000).unwrap();
        setup.shown(121_000);
        setup.choose(0, 121_001, 121_001);
        assert_eq!(setup.take(121_002), Err(Error::Expired));

        let mut timely = shown();
        timely.choose(0, 120_998, 120_999);
        assert_eq!(timely.take(120_999).unwrap().stable_id(), "n-a");
    }

    #[test]
    fn clock_regression_and_deadline_overflow_refuse() {
        assert_eq!(
            Choice::new(catalog(), u64::MAX).err(),
            Some(Error::ClockChanged)
        );
        let mut choice = shown();
        choice.choose(0, 1_020, 1_021);
        assert_eq!(choice.take(1_019), Err(Error::ClockChanged));
    }

    #[test]
    fn wrapped_message_time_keeps_the_original_visible_catalog() {
        let wrap = 1_u64 << 32;
        let mut choice = Choice::new(catalog(), wrap - 100).unwrap();
        choice.shown(wrap - 10);
        choice.choose(1, 5, wrap + 6);
        assert_eq!(choice.take(wrap + 7).unwrap().stable_id(), "n-z");
    }

    #[test]
    fn repeated_show_and_selection_do_not_replace_the_original_choice() {
        let mut choice = shown();
        choice.shown(1_015);
        choice.choose(0, 1_012, 1_020);
        choice.choose(1, 1_021, 1_022);
        assert_eq!(choice.take(1_023).unwrap().stable_id(), "n-a");
    }
}
