//! Original, bounded console-session evidence. Public Apple session properties
//! do not establish graphical screen-lock state; `Active` never means unlocked.
#![forbid(unsafe_code)]

pub(crate) const OPEN_NS: u64 = 2_000_000_000;
/// Evidence lifetime from read START. Under load on a Mac, one
/// `CGSessionCopyCurrentDictionary` call took up to 571 ms, so a 500 ms
/// lifetime ended every running session at once, about every 17-40 minutes
/// (fr-uvji). Any stall shorter than this lifetime is now ridden out. A
/// logout or user switch that `WindowServer` has not yet reported takes
/// effect at most this late.
pub(crate) const VALID_NS: u64 = 2_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    Inactive,
    Suspending,
    SessionUnavailable,
    IdentityChanged,
    ServiceUnavailable,
    UnsupportedSession,
    EvidenceExpired,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Opening,
    Active { until_ns: u64 },
    Stopped(Stop),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) uid: u32,
    pub(crate) security_session: u32,
    pub(crate) console_set: u32,
}

#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct Observation {
    pub(crate) identity: Identity,
    pub(crate) on_console: bool,
    pub(crate) login_done: bool,
    pub(crate) graphic_access: bool,
    pub(crate) protected_data_available: bool,
    pub(crate) display_awake: bool,
}

/// One helper process epoch. A negative signal or expired read permanently
/// stops it; later observations, switch-in and wake cannot reacquire custody.
pub(crate) struct Evidence {
    uid: u32,
    born: u64,
    last: u64,
    until: u64,
    identity: Option<Identity>,
    status: Status,
}

impl Evidence {
    pub(crate) fn new(uid: u32, now: Option<u64>) -> Self {
        let started = now.unwrap_or(0);
        let until = started.checked_add(OPEN_NS);
        Self {
            uid,
            born: started,
            last: started,
            until: until.unwrap_or(0),
            identity: None,
            status: if uid == 0 {
                Status::Stopped(Stop::UnsupportedSession)
            } else if now.is_none() || until.is_none() {
                Status::Stopped(Stop::Failed)
            } else {
                Status::Opening
            },
        }
    }

    pub(crate) const fn uid(&self) -> u32 {
        self.uid
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
            return self.stop(Stop::EvidenceExpired);
        }
        self.status
    }

    pub(crate) fn observe(
        &mut self,
        read_started: u64,
        now: Option<u64>,
        observation: Observation,
    ) -> Status {
        if let stopped @ Status::Stopped(_) = self.status(now) {
            return stopped;
        }
        let Some(now) = now else {
            return self.stop(Stop::Failed);
        };
        if read_started < self.born || read_started > now {
            return self.stop(Stop::Failed);
        }
        let Some(until) = read_started.checked_add(VALID_NS) else {
            return self.stop(Stop::Failed);
        };
        // Stamp before the native call, never on reply receipt. A stalled call
        // cannot return and convert stale evidence into another lease.
        if until <= now {
            return self.stop(Stop::EvidenceExpired);
        }
        if observation.identity.uid != self.uid
            || self
                .identity
                .is_some_and(|bound| bound != observation.identity)
        {
            return self.stop(Stop::IdentityChanged);
        }
        if !observation.graphic_access
            || [0, u32::MAX - 1, u32::MAX].contains(&observation.identity.security_session)
        {
            return self.stop(Stop::UnsupportedSession);
        }
        if !observation.login_done || !observation.protected_data_available {
            return self.stop(Stop::SessionUnavailable);
        }
        if !observation.on_console {
            return self.stop(Stop::Inactive);
        }
        if !observation.display_awake {
            return self.stop(Stop::Suspending);
        }
        self.identity = Some(observation.identity);
        self.until = until;
        self.status = Status::Active { until_ns: until };
        self.status
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn observation() -> Observation {
        Observation {
            identity: Identity {
                uid: 501,
                security_session: 140,
                console_set: 0,
            },
            on_console: true,
            login_done: true,
            graphic_access: true,
            protected_data_available: true,
            display_awake: true,
        }
    }

    #[test]
    fn opening_never_authorizes_and_expires_at_the_original_boundary() {
        let mut evidence = Evidence::new(501, Some(10));
        assert_eq!(evidence.uid(), 501);
        assert_eq!(evidence.status(Some(10)), Status::Opening);
        assert_eq!(evidence.status(Some(10 + OPEN_NS - 1)), Status::Opening);
        assert_eq!(
            evidence.status(Some(10 + OPEN_NS)),
            Status::Stopped(Stop::EvidenceExpired)
        );
        assert_eq!(
            evidence.observe(10 + OPEN_NS, Some(11 + OPEN_NS), observation()),
            Status::Stopped(Stop::EvidenceExpired)
        );
    }

    #[test]
    fn real_reads_keep_their_original_deadline_not_their_receipt_time() {
        let mut evidence = Evidence::new(501, Some(10));
        assert_eq!(
            evidence.observe(20, Some(100), observation()),
            Status::Active {
                until_ns: 20 + VALID_NS
            }
        );
        assert!(matches!(
            evidence.status(Some(20 + VALID_NS - 1)),
            Status::Active { .. }
        ));
        assert_eq!(
            evidence.status(Some(20 + VALID_NS)),
            Status::Stopped(Stop::EvidenceExpired)
        );
    }

    #[test]
    fn a_native_read_that_stalls_cannot_publish_its_late_result() {
        let mut evidence = Evidence::new(501, Some(0));
        assert_eq!(
            evidence.observe(1, Some(1 + VALID_NS), observation()),
            Status::Stopped(Stop::EvidenceExpired)
        );
        assert_eq!(
            evidence.observe(1 + VALID_NS, Some(2 + VALID_NS), observation()),
            Status::Stopped(Stop::EvidenceExpired)
        );
    }

    #[test]
    fn bounded_same_identity_observations_can_refresh_before_expiry() {
        let mut evidence = Evidence::new(501, Some(0));
        for at in [1, 100_000_000, 200_000_000, 600_000_000] {
            assert_eq!(
                evidence.observe(at, Some(at + 10), observation()),
                Status::Active {
                    until_ns: at + VALID_NS
                }
            );
        }
    }

    #[test]
    fn uid_security_session_and_console_identity_are_bound_once() {
        for change in 0..3 {
            let mut evidence = Evidence::new(501, Some(0));
            evidence.observe(1, Some(2), observation());
            let mut changed = observation();
            match change {
                0 => changed.identity.uid += 1,
                1 => changed.identity.security_session += 1,
                _ => changed.identity.console_set += 1,
            }
            assert_eq!(
                evidence.observe(3, Some(4), changed),
                Status::Stopped(Stop::IdentityChanged)
            );
            assert_eq!(
                evidence.observe(5, Some(6), observation()),
                Status::Stopped(Stop::IdentityChanged)
            );
        }
        let mut wrong_user = Evidence::new(502, Some(0));
        assert_eq!(
            wrong_user.observe(1, Some(2), observation()),
            Status::Stopped(Stop::IdentityChanged)
        );
    }

    #[test]
    fn console_logout_graphic_and_protected_data_refusals_are_terminal() {
        for (change, reason) in [
            (0, Stop::Inactive),
            (1, Stop::SessionUnavailable),
            (2, Stop::UnsupportedSession),
            (3, Stop::SessionUnavailable),
        ] {
            let mut evidence = Evidence::new(501, Some(0));
            let mut changed = observation();
            match change {
                0 => changed.on_console = false,
                1 => changed.login_done = false,
                2 => changed.graphic_access = false,
                _ => changed.protected_data_available = false,
            }
            assert_eq!(
                evidence.observe(1, Some(2), changed),
                Status::Stopped(reason)
            );
            assert_eq!(
                evidence.observe(3, Some(4), observation()),
                Status::Stopped(reason)
            );
        }
    }

    #[test]
    fn a_sleeping_display_never_opens_or_resumes_an_original_session() {
        for was_active in [false, true] {
            let mut evidence = Evidence::new(501, Some(0));
            if was_active {
                assert!(matches!(
                    evidence.observe(1, Some(2), observation()),
                    Status::Active { .. }
                ));
            }
            let mut sleeping = observation();
            sleeping.display_awake = false;
            assert_eq!(
                evidence.observe(3, Some(4), sleeping),
                Status::Stopped(Stop::Suspending)
            );
            // An independently observed wake cannot restore this monitor epoch.
            assert_eq!(
                evidence.observe(5, Some(6), observation()),
                Status::Stopped(Stop::Suspending)
            );
        }
    }

    #[test]
    fn negative_notifications_are_sticky_and_never_grant_evidence() {
        for reason in [
            Stop::Inactive,
            Stop::Suspending,
            Stop::SessionUnavailable,
            Stop::ServiceUnavailable,
            Stop::Failed,
        ] {
            let mut evidence = Evidence::new(501, Some(0));
            assert_eq!(evidence.stop(reason), Status::Stopped(reason));
            assert_eq!(evidence.stop(Stop::Failed), Status::Stopped(reason));
            assert_eq!(
                evidence.observe(1, Some(2), observation()),
                Status::Stopped(reason)
            );
        }
    }

    #[test]
    fn sleep_clock_gap_expires_even_if_uptime_or_later_native_state_is_unchanged() {
        let mut evidence = Evidence::new(501, Some(0));
        evidence.observe(1, Some(2), observation());
        // The production input is Darwin CLOCK_MONOTONIC, which includes sleep.
        assert_eq!(
            evidence.observe(VALID_NS + 1, Some(VALID_NS + 2), observation()),
            Status::Stopped(Stop::EvidenceExpired)
        );
    }

    #[test]
    fn missing_regressing_future_and_overflowed_clocks_never_authorize() {
        for now in [None, Some(u64::MAX)] {
            assert!(matches!(
                Evidence::new(501, now).status(now),
                Status::Stopped(_)
            ));
        }
        let mut regressed = Evidence::new(501, Some(10));
        assert_eq!(regressed.status(Some(9)), Status::Stopped(Stop::Failed));
        let mut future = Evidence::new(501, Some(10));
        assert_eq!(
            future.observe(12, Some(11), observation()),
            Status::Stopped(Stop::Failed)
        );
        let mut absent = Evidence::new(501, Some(10));
        assert_eq!(absent.status(None), Status::Stopped(Stop::Failed));
    }

    #[test]
    fn root_and_placeholder_security_sessions_are_not_console_identity() {
        assert!(matches!(
            Evidence::new(0, Some(0)).status(Some(1)),
            Status::Stopped(Stop::UnsupportedSession)
        ));
        for id in [0, u32::MAX - 1, u32::MAX] {
            let mut evidence = Evidence::new(501, Some(0));
            let mut invalid = observation();
            invalid.identity.security_session = id;
            assert_eq!(
                evidence.observe(1, Some(2), invalid),
                Status::Stopped(Stop::UnsupportedSession)
            );
        }
    }
}
