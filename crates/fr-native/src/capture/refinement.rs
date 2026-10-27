//! One optional encode after a genuinely stationary source, never an idle timer
//! producing video forever. Both the parent's observation clock and native
//! monotonic time must witness the dwell; missing observations break it.
use std::time::{Duration, Instant};

const SETTLE: Duration = Duration::from_millis(400);
const EVIDENCE_GAP: Duration = Duration::from_millis(250);
// Keep the existing 200 ms VBV target and admit at most one optional picture per
// two seconds. This reserves the other capture opportunities for damage/recovery;
// actual source/transport credit and the access-unit byte cap remain unchanged.
const COOLDOWN: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
pub(super) struct Moment {
    host_us: u64,
    worker: Instant,
}
impl Moment {
    pub(super) const fn new(host_us: u64, worker: Instant) -> Self {
        Self { host_us, worker }
    }
    fn elapsed(self, earlier: Self) -> Option<(Duration, Duration)> {
        Some((
            Duration::from_micros(self.host_us.checked_sub(earlier.host_us)?),
            self.worker.checked_duration_since(earlier.worker)?,
        ))
    }
    fn after(self, earlier: Self, duration: Duration) -> bool {
        self.elapsed(earlier)
            .is_some_and(|(host, worker)| host >= duration && worker >= duration)
    }
    fn follows(self, earlier: Self) -> bool {
        self.elapsed(earlier)
            .is_some_and(|(host, worker)| host <= EVIDENCE_GAP && worker <= EVIDENCE_GAP)
    }
}

#[derive(Default)]
pub(super) struct Refinement {
    armed: bool,
    first: Option<Moment>,
    last: Option<Moment>,
    admitted: Option<Moment>,
}
impl Refinement {
    pub(super) fn changed(&mut self) {
        self.armed = true;
        self.first = None;
        self.last = None;
        // Damage cannot replenish the source's global optional-work allowance.
    }
    pub(super) fn unchanged(&mut self, now: Moment) {
        if self.last.is_none_or(|last| !now.follows(last)) {
            self.first = Some(now);
        }
        self.last = Some(now);
    }
    /// Advisory only. The caller must freshly read and compare the full source
    /// before using this answer, and must still own original capture credit.
    pub(super) fn due(&self, now: Moment) -> bool {
        self.armed
            && self.first.is_some_and(|first| now.after(first, SETTLE))
            && self.last.is_some_and(|last| now.follows(last))
            && self.admitted.is_none_or(|last| now.after(last, COOLDOWN))
    }
    pub(super) fn admitted(&mut self, now: Moment) {
        self.armed = false;
        self.first = None;
        self.last = None;
        self.admitted = Some(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, host: u64, worker: u64) -> Moment {
        Moment::new(host, base + Duration::from_micros(worker))
    }
    fn observe(state: &mut Refinement, base: Instant, start: u64, end: u64) {
        for t in (start..=end).step_by(100_000) {
            state.unchanged(at(base, t, t));
        }
    }
    #[test]
    fn a_real_damage_burst_gets_one_refinement_and_silence_never_rearms_it() {
        let base = Instant::now();
        let mut state = Refinement::default();
        observe(&mut state, base, 0, 500_000);
        assert!(!state.due(at(base, 500_000, 500_000)));
        state.changed();
        observe(&mut state, base, 600_000, 900_000);
        assert!(!state.due(at(base, 999_999, 999_999)));
        assert!(state.due(at(base, 1_000_000, 1_000_000)));
        state.admitted(at(base, 1_000_000, 1_000_000));
        observe(&mut state, base, 1_100_000, 10_000_000);
        assert!(!state.due(at(base, 10_000_000, 10_000_000)));
    }
    #[test]
    fn a_second_burst_and_repeated_eligibility_reads_do_not_buy_cooldown() {
        let base = Instant::now();
        let mut state = Refinement::default();
        state.changed();
        observe(&mut state, base, 0, 400_000);
        for _ in 0..10 {
            assert!(state.due(at(base, 400_000, 400_000)));
        }
        state.admitted(at(base, 400_000, 400_000));
        state.changed();
        observe(&mut state, base, 500_000, 2_300_000);
        assert!(!state.due(at(base, 2_399_999, 2_399_999)));
        assert!(state.due(at(base, 2_400_000, 2_400_000)));
    }
    #[test]
    fn both_original_clocks_must_witness_dwell_and_gaps_reset_evidence() {
        let base = Instant::now();
        for frozen_host in [false, true] {
            let mut state = Refinement::default();
            state.changed();
            for t in (0..=600_000).step_by(100_000) {
                let now = if frozen_host {
                    at(base, 0, t)
                } else {
                    at(base, t, 0)
                };
                state.unchanged(now);
                assert!(!state.due(now));
            }
        }
        for worker_gap in [false, true] {
            let mut state = Refinement::default();
            state.changed();
            observe(&mut state, base, 0, 400_000);
            let now = if worker_gap {
                at(base, 500_000, 650_001)
            } else {
                at(base, 650_001, 500_000)
            };
            assert!(!state.due(now));
            state.unchanged(now);
            assert!(!state.due(now));
        }
    }
}
