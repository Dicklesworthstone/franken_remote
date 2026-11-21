//! One original observation slot, fenced independently of reader-held locks.
use fr_client::robot::{
    live::{Observation, Phase, Snapshot},
    snapshot::Handle,
};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

const OPEN: u8 = 0;
const BEGINNING: u8 = 1;
const CLEANING: u8 = 2;
const RECONNECTING: u8 = 3;
const STOPPED: u8 = 4;
const EXHAUSTED: u8 = 5;
const REVISION: u64 = 1 << 16;

/// Revision, attempt and retirement phase change as ONE atomic value. In
/// particular, repeated retirement to the same phase cannot cause an ABA.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Gate(u64);

impl Gate {
    fn attempt(self) -> u8 {
        self.0.to_le_bytes()[1]
    }

    fn state(self) -> u8 {
        self.0.to_le_bytes()[0]
    }

    fn live(self) -> bool {
        self.state() == OPEN && self.attempt() != 0
    }

    fn next(self, attempt: u8, state: u8) -> Option<Self> {
        if self.state() == EXHAUSTED {
            return None;
        }
        let revision = (self.0 & !(REVISION - 1)).checked_add(REVISION)?;
        Some(Self(
            revision | (u64::from(attempt) << 8) | u64::from(state),
        ))
    }

    fn retired(self, phase: Phase) -> Self {
        if self.state() == EXHAUSTED {
            return self;
        }
        let state = match (self.attempt(), phase) {
            (1..=32, Phase::Cleaning) => CLEANING,
            (1..=32, Phase::Reconnecting) => RECONNECTING,
            _ => STOPPED,
        };
        self.next(self.attempt(), state)
            .unwrap_or(Self((self.0 & !0xff) | u64::from(EXHAUSTED)))
    }

    fn closed(self) -> Option<Snapshot> {
        let phase = match self.state() {
            OPEN => return None,
            BEGINNING => Phase::Connecting,
            CLEANING => Phase::Cleaning,
            RECONNECTING => Phase::Reconnecting,
            _ => Phase::Stopped,
        };
        Some(Snapshot {
            schema_version: 1,
            attempt: self.attempt(),
            phase,
            frame: None,
        })
    }
}

#[derive(Clone, Default)]
pub(crate) struct Publisher(
    pub(super) Arc<Mutex<Observation>>,
    pub(super) Arc<Mutex<Option<Handle>>>,
    Arc<AtomicU64>,
);

impl Publisher {
    fn gate(&self) -> Gate {
        Gate(self.2.load(Ordering::SeqCst))
    }

    /// Only an explicit newer attempt can open a retired publication slot.
    /// Neither mutex is waited on. The final compare prevents a retirement
    /// during preparation from being overwritten by a late positive result.
    pub fn begin(&self, attempt: u8) -> Result<(), ()> {
        let before = self.gate();
        if !(1..=32).contains(&attempt)
            || attempt <= before.attempt()
            || before.live()
            || before.state() == BEGINNING
            || before.state() == EXHAUSTED
        {
            return Err(());
        }
        let mut observation = self.0.try_lock().map_err(|_| ())?;
        let mut handle = self.1.try_lock().map_err(|_| ())?;
        let Some(reserved) = before.next(attempt, BEGINNING) else {
            self.retire_if_current(before, Phase::Stopped);
            return Err(());
        };
        self.2
            .compare_exchange(before.0, reserved.0, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| ())?;
        let initialized = observation.begin(attempt).is_ok();
        // A contended retirement may have left this handle stored, but its
        // closed gate made it unreadable. Never carry it into the next attempt.
        let old = handle.take();
        let activated = initialized
            && reserved.next(attempt, OPEN).is_some_and(|active| {
                self.2
                    .compare_exchange(reserved.0, active.0, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            });
        if !activated {
            self.retire_if_current(reserved, Phase::Stopped);
        }
        drop(handle);
        drop(observation);
        drop(old);
        if activated { Ok(()) } else { Err(()) }
    }

    /// Session-thread work never waits for a reader-held lock. The caller's
    /// action cannot reopen a retired generation or override a concurrent stop.
    pub fn update(
        &self,
        action: impl FnOnce(&mut Observation) -> Result<(), fr_client::robot::live::Error>,
    ) -> Result<(), ()> {
        let before = self.gate();
        if !before.live() {
            return Err(());
        }
        let Ok(mut observation) = self.0.try_lock() else {
            self.retire_if_current(before, Phase::Stopped);
            return Err(());
        };
        if self.gate() != before {
            return Err(());
        }
        if action(&mut observation).is_err() {
            self.retire_if_current(before, Phase::Stopped);
            return Err(());
        }
        if self.gate() == before {
            Ok(())
        } else {
            Err(())
        }
    }

    pub fn bind_snapshot(&self, handle: Handle) -> Result<(), ()> {
        let before = self.gate();
        if !before.live() {
            return Err(());
        }
        let mut slot = self.1.try_lock().map_err(|_| ())?;
        if self.gate() != before {
            return Err(());
        }
        let old = slot.replace(handle);
        drop(slot);
        drop(old);
        if self.gate() == before {
            Ok(())
        } else {
            Err(())
        }
    }

    /// Publish negative lifecycle evidence BEFORE trying either shared lock.
    /// Readers can always see this phase with no frame, even if a reader or
    /// writer still holds metadata from the retired generation.
    pub fn retire(&self, phase: Phase) {
        let previous = self
            .2
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |word| {
                Some(Gate(word).retired(phase).0)
            })
            .expect("retirement always supplies a replacement gate");
        self.clear_retired(Gate(previous).retired(phase));
    }

    /// A failed old writer may stop only the generation it actually borrowed.
    fn retire_if_current(&self, before: Gate, phase: Phase) {
        let retired = before.retired(phase);
        if self
            .2
            .compare_exchange(before.0, retired.0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.clear_retired(retired);
        }
    }

    fn clear_retired(&self, retired: Gate) {
        if let Ok(mut observation) = self.0.try_lock()
            && self.gate() == retired
            && let Some(snapshot) = retired.closed()
        {
            observation.retire(snapshot.phase);
        }
        // An old retirement cannot clear a newer attempt's source handle.
        // Drop any removed handle outside its lock; no image work happens here.
        let old = self
            .1
            .try_lock()
            .ok()
            .and_then(|mut handle| (self.gate() == retired).then(|| handle.take()).flatten());
        drop(old);
    }

    pub(super) fn read(&self) -> Option<Snapshot> {
        let before = self.gate();
        if let Some(snapshot) = before.closed() {
            return Some(snapshot);
        }
        let snapshot = self.0.try_lock().ok()?.snapshot(Instant::now()).ok()?;
        let after = self.gate();
        if after != before {
            // Never return the copied frame after an intervening retirement.
            // A newly active attempt must be read in its own subsequent call.
            return after.closed();
        }
        if snapshot.attempt != before.attempt() {
            return None;
        }
        snapshot.validate().ok()?;
        Some(snapshot)
    }
}

#[cfg(test)]
#[path = "publisher/tests.rs"]
mod tests;
