//! Bounded evidence from the original macOS observation indicator. Only a
//! completed native visibility/event turn can publish positive evidence.
#![forbid(unsafe_code)]

pub(crate) const OPEN_NS: u64 = 2_000_000_000;
pub(crate) const VALID_NS: u64 = 250_000_000;

/// Why the original UI lifetime ended. Only an attributed local click is a
/// user stop; ordinary invisibility or a stalled event loop is a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    Local,
    AuthorityEnded,
    Failed,
    Expired,
}

/// Read-only evidence; it neither approves a peer nor authorizes observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Opening,
    Shown,
    Stopped(Stop),
}

pub(crate) struct Evidence {
    born: u64,
    last: u64,
    until: u64,
    last_started: Option<u64>,
    status: Status,
}

impl Evidence {
    pub(crate) fn new(now: Option<u64>) -> Self {
        let born = now.unwrap_or(0);
        let until = born.checked_add(OPEN_NS);
        Self {
            born,
            last: born,
            until: until.unwrap_or(0),
            last_started: None,
            status: if now.is_none() || until.is_none() {
                Status::Stopped(Stop::Failed)
            } else {
                Status::Opening
            },
        }
    }

    pub(crate) fn stop(&mut self, why: Stop) -> Status {
        if !matches!(self.status, Status::Stopped(_)) {
            self.status = Status::Stopped(why);
        }
        self.status
    }

    pub(crate) fn status(&mut self, now: Option<u64>) -> Status {
        if matches!(self.status, Status::Stopped(_)) {
            return self.status;
        }
        let Some(now) = now else {
            return self.stop(Stop::Failed);
        };
        if now < self.last {
            return self.stop(Stop::Failed);
        }
        self.last = now;
        if now >= self.until {
            return self.stop(Stop::Expired);
        }
        self.status
    }

    pub(crate) fn complete(&mut self, started: u64, now: Option<u64>, native: Status) -> Status {
        if let stopped @ Status::Stopped(_) = self.status(now) {
            return stopped;
        }
        if let Status::Stopped(why) = native {
            return self.stop(why);
        }
        let Some(now) = now else {
            return self.stop(Stop::Failed);
        };
        if started < self.born || started > now {
            return self.stop(Stop::Failed);
        }
        if native == Status::Opening {
            return if self.status == Status::Opening {
                // Repeated opening turns never move the original open limit.
                Status::Opening
            } else {
                self.stop(Stop::Failed)
            };
        }
        let Some(until) = started.checked_add(VALID_NS) else {
            return self.stop(Stop::Failed);
        };
        if now >= until {
            return self.stop(Stop::Expired);
        }
        // The visibility observation retains its pre-native-call timestamp.
        // A delayed AppKit return cannot receive another quarter-second lease.
        self.until = until;
        self.last_started = Some(started);
        self.status = Status::Shown;
        self.status
    }

    /// A Ready reply must cover a native turn STARTED after its own request.
    /// The parent uses a request-based deadline, so cached readiness alone
    /// could otherwise extend the original source observation's lifetime.
    pub(crate) fn ready_since(&mut self, floor: u64, now: Option<u64>) -> bool {
        self.status(now) == Status::Shown
            && self.last_started.is_some_and(|started| started >= floor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unopened_panels_never_become_visible_and_opening_has_one_budget() {
        let mut evidence = Evidence::new(Some(10));
        assert_eq!(evidence.status(Some(10)), Status::Opening);
        assert_eq!(
            evidence.complete(20, Some(30), Status::Opening),
            Status::Opening
        );
        assert_eq!(evidence.status(Some(10 + OPEN_NS - 1)), Status::Opening);
        assert_eq!(
            evidence.complete(10 + OPEN_NS, Some(10 + OPEN_NS), Status::Shown),
            Status::Stopped(Stop::Expired)
        );
    }

    #[test]
    fn a_real_turn_keeps_its_start_time_and_exclusive_expiry() {
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(
            evidence.complete(10, Some(50), Status::Shown),
            Status::Shown
        );
        assert_eq!(evidence.status(Some(10 + VALID_NS - 1)), Status::Shown);
        assert_eq!(
            evidence.status(Some(10 + VALID_NS)),
            Status::Stopped(Stop::Expired)
        );
    }

    #[test]
    fn a_stalled_native_call_cannot_create_its_first_readiness() {
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(
            evidence.complete(10, Some(10 + VALID_NS), Status::Shown),
            Status::Stopped(Stop::Expired)
        );
        assert_eq!(
            evidence.complete(20 + VALID_NS, Some(30 + VALID_NS), Status::Shown),
            Status::Stopped(Stop::Expired)
        );
    }

    #[test]
    fn sleep_or_a_gap_before_the_next_turn_is_terminal() {
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(evidence.complete(1, Some(2), Status::Shown), Status::Shown);
        assert_eq!(
            evidence.complete(1 + VALID_NS, Some(2 + VALID_NS), Status::Shown),
            Status::Stopped(Stop::Expired)
        );
    }

    #[test]
    fn readers_cannot_extend_a_native_observation() {
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(
            evidence.complete(10, Some(20), Status::Shown),
            Status::Shown
        );
        for elapsed in [1, 100, 1_000, VALID_NS - 1] {
            assert_eq!(evidence.status(Some(10 + elapsed.max(10))), Status::Shown);
        }
        assert_eq!(
            evidence.status(Some(10 + VALID_NS)),
            Status::Stopped(Stop::Expired)
        );
    }

    #[test]
    fn a_new_completed_turn_can_renew_only_a_still_live_original_panel() {
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(evidence.complete(1, Some(2), Status::Shown), Status::Shown);
        assert_eq!(
            evidence.complete(VALID_NS - 1, Some(VALID_NS), Status::Shown),
            Status::Shown
        );
        assert_eq!(evidence.status(Some(2 * VALID_NS - 2)), Status::Shown);
        assert_eq!(
            evidence.status(Some(2 * VALID_NS - 1)),
            Status::Stopped(Stop::Expired)
        );
    }

    #[test]
    fn every_stop_preserves_its_first_reason_and_cannot_reopen() {
        for why in [
            Stop::Local,
            Stop::AuthorityEnded,
            Stop::Failed,
            Stop::Expired,
        ] {
            let mut evidence = Evidence::new(Some(0));
            assert_eq!(evidence.complete(1, Some(2), Status::Shown), Status::Shown);
            assert_eq!(evidence.stop(why), Status::Stopped(why));
            assert_eq!(evidence.stop(Stop::Failed), Status::Stopped(why));
            assert_eq!(evidence.status(None), Status::Stopped(why));
            assert_eq!(
                evidence.complete(3, Some(4), Status::Shown),
                Status::Stopped(why)
            );
        }
    }

    #[test]
    fn native_visibility_loss_cannot_become_opening_again() {
        let mut evidence = Evidence::new(Some(0));
        assert_eq!(evidence.complete(1, Some(2), Status::Shown), Status::Shown);
        assert_eq!(
            evidence.complete(3, Some(4), Status::Opening),
            Status::Stopped(Stop::Failed)
        );
        let mut clicked = Evidence::new(Some(0));
        assert_eq!(
            clicked.complete(1, Some(2), Status::Stopped(Stop::Local)),
            Status::Stopped(Stop::Local)
        );
    }

    #[test]
    fn a_request_requires_a_native_turn_started_after_its_own_receipt() {
        let mut evidence = Evidence::new(Some(0));
        assert!(!evidence.ready_since(1, Some(1)));
        assert_eq!(
            evidence.complete(10, Some(30), Status::Shown),
            Status::Shown
        );
        assert!(
            !evidence.ready_since(20, Some(30)),
            "finishing after receipt is insufficient"
        );
        assert!(evidence.ready_since(10, Some(30)));
        assert_eq!(
            evidence.complete(31, Some(40), Status::Shown),
            Status::Shown
        );
        assert!(evidence.ready_since(20, Some(40)));
        assert!(!evidence.ready_since(41, Some(40)));
        assert!(!evidence.ready_since(20, Some(31 + VALID_NS)));
        assert!(!evidence.ready_since(0, Some(31 + VALID_NS)));
    }

    #[test]
    fn missing_regressed_or_overflowing_clocks_refuse() {
        for now in [None, Some(u64::MAX), Some(u64::MAX - OPEN_NS + 1)] {
            assert_eq!(
                Evidence::new(now).status(now),
                Status::Stopped(Stop::Failed)
            );
        }
        let mut missing = Evidence::new(Some(1));
        assert_eq!(missing.status(None), Status::Stopped(Stop::Failed));
        let mut regressed = Evidence::new(Some(1));
        assert_eq!(regressed.status(Some(0)), Status::Stopped(Stop::Failed));
        for (started, now) in [(0, 2), (3, 2)] {
            let mut evidence = Evidence::new(Some(1));
            assert_eq!(
                evidence.complete(started, Some(now), Status::Shown),
                Status::Stopped(Stop::Failed)
            );
        }
    }
}
