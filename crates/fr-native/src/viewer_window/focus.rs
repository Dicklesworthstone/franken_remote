//! Optional focus evidence for an automation-only owner. The existing window
//! thread owns every native query; readers never block on it or consume input.
use super::{Native, StopReason, fr_viewer_window_focus_begin, fr_viewer_window_focus_poll};
use std::{
    sync::{
        OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const WAIT: Duration = Duration::from_secs(30);
const MAX_AGE: Duration = Duration::from_millis(100);
const INTERVAL: Duration = Duration::from_millis(20);

#[derive(Default)]
pub(super) struct Gate {
    requested: OnceLock<Instant>,
    // Microseconds from the immutable request clock, plus one; zero is unknown.
    sampled: AtomicU64,
    established: AtomicBool,
    stopped: AtomicBool,
}
impl Gate {
    pub(super) fn enable(&self, now: Instant) -> bool {
        !self.stopped.load(Ordering::Acquire)
            && self.requested.set(now).is_ok()
            && !self.stopped.load(Ordering::Acquire)
    }
    pub(super) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.sampled.store(0, Ordering::Release);
    }
    fn elapsed(&self, now: Instant) -> Option<Duration> {
        now.checked_duration_since(*self.requested.get()?)
    }
    pub(super) fn ready(&self, now: Instant) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return false;
        }
        let Some(sampled) = self.sampled.load(Ordering::Acquire).checked_sub(1) else {
            return false;
        };
        self.elapsed(now)
            .and_then(|age| age.checked_sub(Duration::from_micros(sampled)))
            .is_some_and(|age| age < MAX_AGE)
            && !self.stopped.load(Ordering::Acquire)
    }
    fn check_wait(&self, now: Instant) -> Result<bool, StopReason> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(StopReason::FocusUnavailable);
        }
        if self.requested.get().is_none() {
            return Ok(false);
        }
        let elapsed = self.elapsed(now).ok_or(StopReason::FocusUnavailable)?;
        if !self.established.load(Ordering::Acquire) && elapsed >= WAIT {
            return Err(StopReason::FocusUnavailable);
        }
        Ok(true)
    }
    pub(super) fn lost(&self) -> Result<(), StopReason> {
        self.sampled.store(0, Ordering::Release);
        if self.established.load(Ordering::Acquire) {
            self.stop();
            Err(StopReason::FocusLost)
        } else {
            Ok(())
        }
    }
    fn observe(&self, focused: bool, sampled: Instant, now: Instant) -> Result<(), StopReason> {
        if !self.check_wait(now)?
            || now
                .checked_duration_since(sampled)
                .is_none_or(|age| age >= MAX_AGE)
        {
            return Err(StopReason::FocusUnavailable);
        }
        if !focused {
            return self.lost();
        }
        let sampled = self
            .elapsed(sampled)
            .and_then(|age| u64::try_from(age.as_micros()).ok())
            .and_then(|value| value.checked_add(1))
            .ok_or(StopReason::FocusUnavailable)?;
        self.established.store(true, Ordering::Release);
        self.sampled.store(sampled, Ordering::Release);
        if self.stopped.load(Ordering::Acquire) {
            self.sampled.store(0, Ordering::Release);
            return Err(StopReason::FocusUnavailable);
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Probe {
    pending: Option<Instant>,
    previous: Option<Instant>,
}
impl Probe {
    pub(super) fn tick(&mut self, native: &Native<'_>) -> Result<(), StopReason> {
        let gate = &native.shared.automation_focus;
        let now = Instant::now();
        if !gate.check_wait(now)? {
            return Ok(());
        }
        if let Some(sampled) = self.pending {
            if now.duration_since(sampled) >= MAX_AGE {
                return Err(StopReason::FocusUnavailable);
            }
            // SAFETY: the existing window thread uniquely owns this connection;
            // poll consumes at most its one fixed-size outstanding focus reply.
            match unsafe { fr_viewer_window_focus_poll(native.handle.as_ptr()) } {
                0 => return Ok(()),
                result @ (1 | 2) => {
                    self.pending = None;
                    // Age from issuance, not arrival; a late reply cannot
                    // fabricate recent evidence after a native/thread stall.
                    gate.observe(result == 2, sampled, Instant::now())?;
                }
                _ => return Err(StopReason::FocusUnavailable),
            }
        }
        if self
            .previous
            .is_none_or(|at| now.duration_since(at) >= INTERVAL)
        {
            let sampled = Instant::now();
            // SAFETY: one query on the sole existing window connection. It
            // changes no event ownership, focus, input, or session lifetime.
            if unsafe { fr_viewer_window_focus_begin(native.handle.as_ptr()) } != 1 {
                return Err(StopReason::FocusUnavailable);
            }
            self.pending = Some(sampled);
            self.previous = Some(sampled);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focus_is_opt_in_bounded_and_aged_from_request_issuance() {
        let gate = Gate::default();
        let at = Instant::now();
        assert_eq!(gate.check_wait(at), Ok(false));
        assert!(!gate.ready(at));
        assert!(gate.enable(at));
        assert!(!gate.enable(at + Duration::from_secs(1)));
        gate.observe(false, at, at + Duration::from_millis(10))
            .unwrap();
        assert!(!gate.ready(at + Duration::from_millis(10)));
        gate.observe(true, at, at + Duration::from_millis(80))
            .unwrap();
        assert!(gate.ready(at + Duration::from_millis(99)));
        assert!(!gate.ready(at + MAX_AGE));
        assert_eq!(
            gate.observe(true, at, at + MAX_AGE),
            Err(StopReason::FocusUnavailable)
        );
    }

    #[test]
    fn unknown_focus_times_out_and_established_loss_never_rearms() {
        let gate = Gate::default();
        let at = Instant::now();
        assert!(gate.enable(at));
        assert_eq!(
            gate.check_wait(at + WAIT),
            Err(StopReason::FocusUnavailable)
        );
        assert_eq!(
            gate.observe(true, at + WAIT, at + WAIT),
            Err(StopReason::FocusUnavailable)
        );
        let gate = Gate::default();
        assert!(gate.enable(at));
        gate.observe(true, at, at).unwrap();
        assert_eq!(gate.lost(), Err(StopReason::FocusLost));
        assert!(!gate.ready(at));
        assert!(!gate.enable(at));
        assert_eq!(
            gate.observe(true, at, at),
            Err(StopReason::FocusUnavailable)
        );
    }

    #[test]
    fn original_stop_and_future_or_pre_request_evidence_never_authorize() {
        let at = Instant::now();
        let gate = Gate::default();
        assert!(gate.enable(at));
        assert_eq!(
            gate.observe(true, at + Duration::from_secs(1), at),
            Err(StopReason::FocusUnavailable)
        );
        assert_eq!(
            gate.observe(true, at - Duration::from_millis(1), at),
            Err(StopReason::FocusUnavailable)
        );
        gate.observe(true, at, at).unwrap();
        gate.stop();
        assert!(!gate.ready(at));
        assert_eq!(
            gate.observe(true, at, at),
            Err(StopReason::FocusUnavailable)
        );
    }
}
