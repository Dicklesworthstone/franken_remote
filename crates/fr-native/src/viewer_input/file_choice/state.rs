//! One explicit local file dialog, owned by the original input producer.
//! No state here grants remote input or changes its original lease/deadlines.
use std::sync::Mutex;

const PAUSE_US: u64 = 2_000_000;
const INTERACTION_US: u64 = 125_000_000;
const RETURN_US: u64 = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Unavailable,
    Idle,
    Requested,
    Pausing,
    Paused,
    Returning,
    Resumed,
    Refused,
    Closed,
}

struct State {
    phase: Phase,
    serial: u64,
    started: u64,
    until: u64,
    stage_until: u64,
}
impl Default for State {
    fn default() -> Self {
        Self {
            phase: Phase::Unavailable,
            serial: 0,
            started: 0,
            until: 0,
            stage_until: 0,
        }
    }
}
impl State {
    fn check(&mut self, now: u64) -> Result<(), ()> {
        if self.phase == Phase::Closed {
            return Err(());
        }
        if matches!(
            self.phase,
            Phase::Requested | Phase::Pausing | Phase::Paused | Phase::Returning
        ) && (now < self.started || now >= self.until || now >= self.stage_until)
        {
            self.phase = Phase::Closed;
            return Err(());
        }
        Ok(())
    }
}

/// Both clocks and state transitions are content-free. Callers use the same
/// suspend-inclusive native clock; tests supply explicit microsecond samples.
#[derive(Default)]
pub(crate) struct Gate(Mutex<State>);
impl Gate {
    pub(crate) fn enable(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase == Phase::Unavailable {
            state.phase = Phase::Idle;
        }
    }

    pub(crate) fn stop(&self) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .phase = Phase::Closed;
    }

    pub(crate) fn request(&self, now: u64) -> Result<u64, ()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.phase != Phase::Idle {
            return Err(());
        }
        let Some(serial) = state.serial.checked_add(1) else {
            state.phase = Phase::Closed;
            return Err(());
        };
        let Some(until) = now.checked_add(INTERACTION_US) else {
            state.phase = Phase::Closed;
            return Err(());
        };
        state.serial = serial;
        state.started = now;
        state.until = until;
        state.stage_until = now + PAUSE_US;
        state.phase = Phase::Requested;
        Ok(serial)
    }

    pub(crate) fn requested(&self, now: u64) -> Result<Option<u64>, ()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.check(now)?;
        Ok((state.phase == Phase::Requested).then_some(state.serial))
    }

    pub(crate) fn phase(&self, serial: u64, now: u64) -> Result<Phase, ()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.check(now)?;
        if serial == 0 || serial != state.serial {
            return Err(());
        }
        Ok(state.phase)
    }

    pub(crate) fn begin(&self, serial: u64, now: u64) -> Result<(), ()> {
        self.transition(serial, now, Phase::Requested, Phase::Pausing)
    }

    pub(crate) fn refuse(&self, serial: u64, now: u64) -> Result<(), ()> {
        self.transition(serial, now, Phase::Requested, Phase::Refused)
    }

    pub(crate) fn paused(&self, serial: u64, now: u64) -> Result<(), ()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.check(now)?;
        if serial != state.serial || state.phase != Phase::Pausing {
            return Err(());
        }
        state.phase = Phase::Paused;
        state.stage_until = state.until;
        Ok(())
    }

    pub(crate) fn resume(&self, serial: u64, now: u64) -> Result<(), ()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.check(now)?;
        if serial != state.serial {
            return Err(());
        }
        if matches!(state.phase, Phase::Returning | Phase::Resumed) {
            return Ok(());
        }
        if state.phase != Phase::Paused {
            return Err(());
        }
        state.stage_until = now.checked_add(RETURN_US).ok_or(())?.min(state.until);
        state.phase = Phase::Returning;
        Ok(())
    }

    pub(crate) fn resumed(&self, serial: u64, now: u64) -> Result<(), ()> {
        self.transition(serial, now, Phase::Returning, Phase::Resumed)
    }

    fn transition(&self, serial: u64, now: u64, before: Phase, after: Phase) -> Result<(), ()> {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.check(now)?;
        if serial == 0 || serial != state.serial || state.phase != before {
            return Err(());
        }
        state.phase = after;
        Ok(())
    }

    /// Only an untouched refusal or positively restored producer releases the
    /// one slot. An abandoned dialog/return permanently fences this owner.
    pub(crate) fn finish(&self, serial: u64) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if serial == state.serial && matches!(state.phase, Phase::Refused | Phase::Resumed) {
            state.phase = Phase::Idle;
            true
        } else {
            state.phase = Phase::Closed;
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn requested() -> (Gate, u64) {
        let gate = Gate::default();
        assert_eq!(gate.request(100), Err(()));
        gate.enable();
        let serial = gate.request(100).unwrap();
        (gate, serial)
    }

    #[test]
    fn original_pause_and_positive_return_are_both_required_before_reuse() {
        let (gate, first) = requested();
        assert_eq!(gate.requested(101), Ok(Some(first)));
        assert_eq!(gate.request(101), Err(()));
        assert_eq!(gate.resume(first, 101), Err(()));
        gate.begin(first, 102).unwrap();
        assert_eq!(gate.requested(103), Ok(None));
        gate.paused(first, 104).unwrap();
        gate.resume(first, 105).unwrap();
        assert_eq!(gate.phase(first, 106), Ok(Phase::Returning));
        gate.resumed(first, 107).unwrap();
        assert!(gate.finish(first));
        let second = gate.request(108).unwrap();
        assert_ne!(first, second);
        assert_eq!(gate.resume(first, 109), Err(()));
        assert_eq!(gate.phase(second, 109), Ok(Phase::Requested));
    }

    #[test]
    fn untouched_held_input_refusal_leaves_the_original_source_usable() {
        let (gate, serial) = requested();
        gate.refuse(serial, 101).unwrap();
        assert_eq!(gate.phase(serial, 102), Ok(Phase::Refused));
        assert!(gate.finish(serial));
        assert!(gate.request(103).is_ok());
    }

    #[test]
    fn every_unfinished_native_stage_fences_on_guard_abandonment() {
        for phase in [
            Phase::Requested,
            Phase::Pausing,
            Phase::Paused,
            Phase::Returning,
        ] {
            let (gate, serial) = requested();
            gate.0.lock().unwrap().phase = phase;
            assert!(!gate.finish(serial), "{phase:?}");
            gate.enable();
            assert_eq!(gate.request(103), Err(()));
        }
    }

    #[test]
    fn original_stage_deadlines_are_exclusive_and_do_not_restart() {
        let (gate, serial) = requested();
        gate.begin(serial, 100 + PAUSE_US - 1).unwrap();
        assert_eq!(gate.paused(serial, 100 + PAUSE_US), Err(()));
        let (gate, serial) = requested();
        gate.begin(serial, 101).unwrap();
        gate.paused(serial, 102).unwrap();
        gate.resume(serial, 103).unwrap();
        gate.resume(serial, 104).unwrap();
        assert_eq!(gate.resumed(serial, 103 + RETURN_US), Err(()));
    }

    #[test]
    fn return_cannot_extend_the_original_dialog_budget_or_reverse_its_clock() {
        let (gate, serial) = requested();
        gate.begin(serial, 101).unwrap();
        gate.paused(serial, 102).unwrap();
        gate.resume(serial, 100 + INTERACTION_US - 1).unwrap();
        assert_eq!(gate.resumed(serial, 100 + INTERACTION_US), Err(()));
        let (gate, serial) = requested();
        assert_eq!(gate.phase(serial, 99), Err(()));
        assert_eq!(gate.phase(serial, 100), Err(()));
    }

    #[test]
    fn stop_wins_over_pause_return_and_later_enable() {
        let (gate, serial) = requested();
        gate.begin(serial, 101).unwrap();
        gate.stop();
        assert_eq!(gate.paused(serial, 102), Err(()));
        assert_eq!(gate.resumed(serial, 103), Err(()));
        assert!(!gate.finish(serial));
        gate.enable();
        assert_eq!(gate.request(104), Err(()));
    }
}
