//! One original native window's geometry receipt. No grant, mapping, queued
//! input or native handle survives it; a new session must qualify its own view.
#![forbid(unsafe_code)]

use std::{cell::Cell, time::Duration};

/// The seal and the reopen must follow the latest native change this closely.
/// A live drag holds `AppKit`'s tracking loop until mouse-up, so a bound from
/// the first change ended every drag slower than this instead of reopening.
const FRESH: Duration = Duration::from_secs(2);
/// A resize cannot keep extending the original cleanup/reopen opportunity.
const MAX_AGE: Duration = Duration::from_secs(30);

/// Plain, bounded metadata shared with the main-thread `AppKit` boundary.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub(crate) struct Sample {
    pub(crate) revision: u64,
    pub(crate) width: f64,
    pub(crate) height: f64,
    pub(crate) x: f64,
    pub(crate) y: f64,
    pub(crate) observed: f64,
    pub(crate) eligible: i32,
    /// 1 when the window is in full screen, else 0.
    pub(crate) full_screen: i32,
}

/// Local window points, not drawable pixels or remote input coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Placement {
    pub(crate) size: (u32, u32),
    pub(crate) origin: (i32, i32),
    /// The new window enters full screen, as the old one ended.
    pub(crate) full_screen: bool,
}

#[derive(Clone, Copy)]
struct Checked {
    revision: u64,
    placement: Placement,
    observed: Duration,
}

impl Sample {
    fn checked(self) -> Option<Checked> {
        if self.revision == 0
            || self.eligible != 1
            || !(0..=1).contains(&self.full_screen)
            || !self.width.is_finite()
            || !self.height.is_finite()
            || !(1.0..=8192.0).contains(&self.width)
            || !(1.0..=8192.0).contains(&self.height)
            || !self.x.is_finite()
            || !self.y.is_finite()
            || !(-1_000_000.0..=1_000_000.0).contains(&self.x)
            || !(-1_000_000.0..=1_000_000.0).contains(&self.y)
        {
            return None;
        }
        let observed = Duration::try_from_secs_f64(self.observed).ok()?;
        if observed.is_zero() {
            return None;
        }
        // All casts are bounded above. Floor the content ceiling; these points
        // only size a new window and never repair the old input mapping.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let placement = Placement {
            size: (self.width.floor() as u32, self.height.floor() as u32),
            origin: (self.x.round() as i32, self.y.round() as i32),
            full_screen: self.full_screen == 1,
        };
        Some(Checked {
            revision: self.revision,
            placement,
            observed,
        })
    }
}

#[derive(Clone, Copy, Default)]
struct State {
    original: Option<Checked>,
    pending: Option<Checked>,
    began: Option<Duration>,
    /// The latest observed change; the seal's sample does not move it.
    latest: Option<Duration>,
    sealed: bool,
    cancelled: bool,
}

impl State {
    fn timely(&self, now: Duration) -> bool {
        let within = |since: Option<Duration>, bound| {
            since.is_some_and(|since| now.checked_sub(since).is_some_and(|age| age < bound))
        };
        within(self.latest, FRESH) && within(self.began, MAX_AGE)
    }
}

/// Main-thread-only scalar state. Native callbacks never borrow a session or
/// hold a lock, and no callback can panic from a reentrant `RefCell` borrow.
#[derive(Default)]
pub(crate) struct Intent(Cell<State>);

impl Intent {
    /// Arm only from the actual first submitted, current native geometry.
    pub(crate) fn arm(&self, sample: Sample) -> bool {
        let mut state = self.0.get();
        if state.original.is_some() || state.cancelled {
            return false;
        }
        state.original = sample.checked();
        self.0.set(state);
        state.original.is_some()
    }

    pub(crate) fn armed(&self) -> bool {
        let state = self.0.get();
        state.original.is_some() || state.cancelled
    }

    pub(crate) fn watching(&self) -> bool {
        let state = self.0.get();
        state.original.is_some() && !state.cancelled && state.began.is_none()
    }

    pub(crate) fn changed(&self) -> bool {
        self.0.get().began.is_some()
    }

    /// The original native layout callback coalesces only newer revisions.
    /// Invalid evidence is terminal; returning to the old size cannot revive it.
    pub(crate) fn observe(&self, sample: Sample, original_stopped: bool) {
        // The first resize must cause the original stop. Coalesced callbacks
        // after our own stop may refine only this same bounded receipt.
        if original_stopped && !self.changed() {
            self.cancel();
            return;
        }
        let Some(current) = sample.checked() else {
            self.cancel();
            return;
        };
        let mut state = self.0.get();
        let previous = state.pending.or(state.original);
        if state.cancelled
            || state.sealed
            || previous.is_none_or(|previous| {
                current.revision <= previous.revision || current.observed < previous.observed
            })
        {
            self.cancel();
            return;
        }
        state.began.get_or_insert(current.observed);
        state.latest = Some(current.observed);
        state.pending = Some(current);
        self.0.set(state);
    }

    /// After a complete native event turn, seal only the exact current size.
    /// Focus/hide/close/failure callbacks can still cancel it before window free.
    pub(crate) fn seal(&self, sample: Sample) -> bool {
        let Some(current) = sample.checked() else {
            self.cancel();
            return false;
        };
        let mut state = self.0.get();
        let valid = !state.cancelled
            && !state.sealed
            && state.pending.is_some_and(|pending| {
                pending.revision == current.revision
                    && pending.placement == current.placement
                    && current.observed >= pending.observed
            })
            && state.timely(current.observed);
        if valid {
            state.sealed = true;
            // Preserve the final event-domain observation so a regressing
            // clock after native teardown cannot consume an older receipt.
            state.pending = Some(current);
            self.0.set(state);
        } else {
            self.cancel();
        }
        valid
    }

    pub(crate) fn cancel(&self) {
        let mut state = self.0.get();
        state.cancelled = true;
        state.pending = None;
        self.0.set(state);
    }

    /// The caller must first confirm all original native owners were reaped,
    /// and reject adverse host reports and unrelated local failures.
    pub(crate) fn take(&self, now: Option<Duration>) -> Option<Placement> {
        let state = self.0.get();
        let timely = now.is_some_and(|now| {
            state.pending.is_some_and(|pending| now >= pending.observed) && state.timely(now)
        });
        let result = (state.sealed && !state.cancelled && timely)
            .then(|| state.pending.map(|sample| sample.placement))
            .flatten();
        self.cancel();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(revision: u64, ms: u32, width: u32, height: u32) -> Sample {
        Sample {
            revision,
            width: f64::from(width),
            height: f64::from(height),
            x: -1920.0,
            y: 40.0,
            observed: f64::from(ms) / 1000.0,
            eligible: 1,
            full_screen: 0,
        }
    }

    fn armed() -> Intent {
        let intent = Intent::default();
        assert!(!intent.armed());
        assert!(!intent.watching());
        assert!(intent.arm(sample(1, 100, 960, 660)));
        assert!(intent.armed());
        assert!(intent.watching());
        intent
    }

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn native_revisions_coalesce_one_size_without_minting_a_mapping() {
        let intent = armed();
        intent.observe(sample(2, 110, 1280, 840), false);
        intent.observe(sample(3, 120, 1440, 930), true);
        assert!(intent.changed());
        assert!(!intent.watching());
        assert!(intent.seal(sample(3, 130, 1440, 930)));
        assert_eq!(
            intent.take(Some(at(140))),
            Some(Placement {
                size: (1440, 930),
                origin: (-1920, 40),
                full_screen: false,
            })
        );
        assert_eq!(intent.take(Some(at(140))), None);
        assert!(!intent.arm(sample(4, 140, 1440, 930)));
    }

    #[test]
    fn a_full_screen_window_reopens_in_full_screen_and_a_reversed_transition_does_not() {
        let full = |revision, ms| Sample {
            full_screen: 1,
            x: 0.0,
            y: 0.0,
            ..sample(revision, ms, 1920, 1080)
        };
        // The window fences at the transition's start (a new revision at the
        // old size) and reports the settled full-screen size afterwards.
        let entered = armed();
        entered.observe(sample(2, 110, 960, 660), false);
        entered.observe(full(3, 800), true);
        assert!(entered.seal(full(3, 820)));
        assert_eq!(
            entered.take(Some(at(900))),
            Some(Placement {
                size: (1920, 1080),
                origin: (0, 0),
                full_screen: true,
            })
        );
        // Leaving full screen before the seal is a different placement.
        let reversed = armed();
        reversed.observe(full(2, 800), false);
        assert!(!reversed.seal(sample(2, 820, 1920, 1080)));
        assert!(reversed.take(Some(at(900))).is_none());
    }

    #[test]
    fn a_backing_scale_change_needs_a_new_revision_even_at_the_same_point_size() {
        let changed = armed();
        changed.observe(sample(2, 110, 960, 660), false);
        assert!(changed.seal(sample(2, 120, 960, 660)));
        assert_eq!(changed.take(Some(at(130))).unwrap().size, (960, 660));
        let repaint = armed();
        repaint.observe(sample(1, 110, 960, 660), false);
        assert!(!repaint.seal(sample(1, 120, 960, 660)));
        assert!(repaint.take(Some(at(130))).is_none());
    }

    #[test]
    fn startup_unconfirmed_and_late_sizes_cannot_become_reopen_receipts() {
        let startup = Intent::default();
        startup.observe(sample(1, 110, 1280, 840), false);
        assert!(!startup.seal(sample(1, 120, 1280, 840)));
        assert!(startup.take(Some(at(130))).is_none());
        let unconfirmed = armed();
        unconfirmed.observe(sample(2, 110, 1280, 840), false);
        assert!(unconfirmed.take(Some(at(130))).is_none());
        let changed_again = armed();
        changed_again.observe(sample(2, 110, 1280, 840), false);
        assert!(!changed_again.seal(sample(3, 120, 1440, 930)));
    }

    #[test]
    fn a_slow_drag_reopens_when_it_ends_but_a_stale_or_endless_one_does_not() {
        // A 3 s drag: AppKit reports sizes throughout and returns at mouse-up.
        let slow = armed();
        for (revision, ms, width) in [
            (2, 110, 1000),
            (3, 1500, 1100),
            (4, 2900, 1200),
            (5, 3100, 1285),
        ] {
            slow.observe(sample(revision, ms, width, 840), false);
        }
        assert!(slow.seal(sample(5, 3150, 1285, 840)));
        assert_eq!(slow.take(Some(at(3300))).unwrap().size, (1285, 840));
        // Sealed or taken 2 s after the latest change is too late.
        let stale = armed();
        stale.observe(sample(2, 110, 1280, 840), false);
        stale.observe(sample(3, 2000, 1440, 930), false);
        assert!(!stale.seal(sample(3, 4000, 1440, 930)));
        let late_take = armed();
        late_take.observe(sample(2, 110, 1280, 840), false);
        late_take.observe(sample(3, 2000, 1440, 930), false);
        assert!(late_take.seal(sample(3, 2110, 1440, 930)));
        assert!(late_take.take(Some(at(4000))).is_none());
        // Changes that keep coming cannot extend the receipt past MAX_AGE.
        let endless = armed();
        for step in 0..=30 {
            endless.observe(
                sample(u64::from(step) + 2, 110 + step * 1000, 1280 + step, 840),
                false,
            );
        }
        assert!(!endless.seal(sample(32, 30_120, 1310, 840)));
        for (now, reopens) in [(30_109, true), (30_110, false)] {
            let long = armed();
            long.observe(sample(2, 110, 1280, 840), false);
            long.observe(sample(3, 29_000, 1440, 930), false);
            assert!(long.seal(sample(3, 29_050, 1440, 930)));
            assert_eq!(long.take(Some(at(now))).is_some(), reopens);
        }
    }

    #[test]
    fn clock_and_revision_regressions_remain_terminal() {
        for late in [sample(3, 109, 1280, 840), sample(1, 120, 1280, 840)] {
            let intent = armed();
            intent.observe(sample(2, 110, 1280, 840), false);
            intent.observe(late, false);
            assert!(!intent.seal(sample(3, 130, 1280, 840)));
        }
    }

    #[test]
    fn unrelated_original_stop_cannot_be_reclassified_by_late_geometry() {
        let intent = armed();
        intent.observe(sample(2, 110, 1280, 840), true);
        assert!(!intent.changed());
        assert!(!intent.watching());
        assert!(!intent.seal(sample(2, 120, 1280, 840)));
        assert!(intent.take(Some(at(130))).is_none());
        intent.observe(sample(3, 140, 1440, 930), false);
        assert!(!intent.seal(sample(3, 150, 1440, 930)));
    }

    #[test]
    fn cleanup_delay_or_clock_rollback_cannot_extend_a_sealed_receipt() {
        for now in [Some(at(119)), Some(at(2110)), Some(at(9999)), None] {
            let intent = armed();
            intent.observe(sample(2, 110, 1280, 840), false);
            assert!(intent.seal(sample(2, 120, 1280, 840)));
            assert!(intent.take(now).is_none());
            // A later valid clock cannot retry an already rejected receipt.
            assert!(intent.take(Some(at(121))).is_none());
        }
        let intent = armed();
        intent.observe(sample(2, 110, 1280, 840), false);
        assert!(intent.seal(sample(2, 120, 1280, 840)));
        assert!(intent.take(Some(at(2109))).is_some());
    }

    #[test]
    fn terminal_native_state_wins_before_and_after_sealing() {
        for sealed in [false, true] {
            let intent = armed();
            intent.observe(sample(2, 110, 1280, 840), false);
            if sealed {
                assert!(intent.seal(sample(2, 120, 1280, 840)));
            }
            intent.cancel();
            assert!(intent.take(Some(at(130))).is_none());
            intent.observe(sample(3, 140, 960, 660), false);
            assert!(!intent.seal(sample(3, 150, 960, 660)));
        }
    }

    #[test]
    fn malformed_native_geometry_never_reaches_a_new_window() {
        let base = sample(2, 110, 1280, 840);
        for invalid in [
            Sample {
                revision: 0,
                ..base
            },
            Sample {
                eligible: 0,
                ..base
            },
            Sample {
                eligible: 2,
                ..base
            },
            Sample {
                full_screen: 2,
                ..base
            },
            Sample {
                full_screen: -1,
                ..base
            },
            Sample {
                width: f64::NAN,
                ..base
            },
            Sample {
                height: f64::INFINITY,
                ..base
            },
            Sample {
                width: 8193.0,
                ..base
            },
            Sample {
                height: 0.0,
                ..base
            },
            Sample {
                x: f64::INFINITY,
                ..base
            },
            Sample {
                y: 1_000_001.0,
                ..base
            },
            Sample {
                observed: 0.0,
                ..base
            },
            Sample {
                observed: -1.0,
                ..base
            },
        ] {
            let intent = armed();
            intent.observe(invalid, false);
            assert!(!intent.seal(sample(2, 120, 1280, 840)));
            assert!(intent.take(Some(at(130))).is_none());
        }
    }
}
