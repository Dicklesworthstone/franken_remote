//! One checked local replacement size. It carries no mapping or authority.
use fr_media::worker::presentation::X11Target;
use std::sync::Mutex;

#[derive(Default)]
struct State {
    limits: Option<(u32, u32)>,
    pending: Option<(u32, u32)>,
    observed: bool,
    sealed: bool,
    cancelled: bool,
}
#[derive(Default)]
pub(super) struct Intent(Mutex<State>);
impl Intent {
    pub(super) fn enable(&self, width: u32, height: u32) -> bool {
        if X11Target::new(1, width, height).is_err() {
            return false;
        }
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled || state.limits.is_some() || state.pending.is_some() {
            return false;
        }
        state.limits = Some((width, height));
        true
    }
    pub(super) fn limits(&self) -> Option<(u32, u32)> {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (!state.cancelled).then_some(state.limits).flatten()
    }
    /// Serialize first-cause reservation with the original tentative request.
    /// reserve must only inspect/reserve scalars, never fence or call native code.
    /// Input may start a fence, but can never replace the window's final size.
    pub(super) fn begin(&self, width: u32, height: u32, reserve: impl FnOnce() -> bool) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled || !valid(&state, width, height) {
            state.pending = None;
            state.cancelled = true;
            return false;
        }
        if state.pending.is_some() {
            return true;
        }
        if state.sealed || !reserve() {
            state.cancelled = true;
            return false;
        }
        state.pending = Some((width, height));
        true
    }
    /// Only the original window connection observes/coalesces these dimensions.
    /// Its event order is authoritative; another connection's delayed event
    /// cannot overwrite this metadata after the native owner has exited.
    pub(super) fn observe(&self, width: u32, height: u32) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled
            || state.sealed
            || state.pending.is_none()
            || !valid(&state, width, height)
        {
            state.pending = None;
            state.cancelled = true;
            return false;
        }
        state.pending = Some((width, height));
        state.observed = true;
        true
    }
    pub(super) fn pending(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .is_some()
    }
    pub(super) fn observed(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .observed
    }
    pub(super) fn seal(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.sealed = true;
        if !state.observed {
            state.pending = None;
            state.cancelled = true;
        }
    }
    pub(super) fn requested(&self) -> Option<(u32, u32)> {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (state.sealed && state.observed && !state.cancelled)
            .then_some(state.pending)
            .flatten()
    }
    pub(super) fn cancel(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.cancelled = true;
        state.pending = None;
    }
}
fn valid(state: &State, width: u32, height: u32) -> bool {
    state.limits.is_some_and(|(w, h)| width <= w && height <= h)
        && X11Target::new(1, width, height).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn input_cannot_replace_the_original_windows_observed_and_sealed_size() {
        let intent = Intent::default();
        assert!(intent.enable(1920, 1080));
        assert!(intent.begin(640, 480, || true));
        assert_eq!(intent.requested(), None);
        assert!(intent.observe(960, 540));
        assert!(intent.observe(1280, 720));
        assert!(intent.begin(640, 480, || panic!("first cause already reserved")));
        intent.seal();
        assert_eq!(intent.requested(), Some((1280, 720)));
        assert!(intent.begin(640, 480, || panic!("late input cannot reserve again")));
        assert_eq!(intent.requested(), Some((1280, 720)));
    }

    #[test]
    fn simultaneous_native_readers_reserve_one_cause_without_cancelling_each_other() {
        let intent = Arc::new(Intent::default());
        assert!(intent.enable(1920, 1080));
        let barrier = Arc::new(Barrier::new(3));
        let reservations = Arc::new(AtomicUsize::new(0));
        std::thread::scope(|scope| {
            let mut tasks = Vec::new();
            for _ in 0..2 {
                let (intent, barrier, reservations) =
                    (intent.clone(), barrier.clone(), reservations.clone());
                tasks.push(scope.spawn(move || {
                    barrier.wait();
                    intent.begin(640, 480, || {
                        reservations.fetch_add(1, Ordering::AcqRel);
                        true
                    })
                }));
            }
            barrier.wait();
            for task in tasks {
                assert!(task.join().unwrap());
            }
        });
        assert_eq!(reservations.load(Ordering::Acquire), 1);
        assert!(intent.observe(960, 540));
        intent.seal();
        assert_eq!(intent.requested(), Some((960, 540)));
    }

    #[test]
    fn unconfirmed_invalid_or_cancelled_sizes_never_reopen() {
        let unconfirmed = Intent::default();
        assert!(unconfirmed.enable(1920, 1080));
        assert!(unconfirmed.begin(640, 480, || true));
        unconfirmed.seal();
        assert_eq!(unconfirmed.requested(), None);
        for size in [(0, 480), (14, 480), (640, 481), (1922, 1080), (8192, 8192)] {
            let intent = Intent::default();
            assert!(intent.enable(1920, 1080));
            assert!(!intent.begin(size.0, size.1, || true));
            assert!(!intent.begin(640, 480, || true));
        }
        let intent = Intent::default();
        assert!(intent.enable(1920, 1080));
        assert!(!intent.begin(640, 480, || false));
        assert!(!intent.begin(640, 480, || true));
        let intent = Intent::default();
        assert!(intent.enable(1920, 1080));
        assert!(intent.begin(640, 480, || true));
        assert!(intent.observe(960, 540));
        intent.seal();
        intent.cancel();
        assert_eq!(intent.requested(), None);
        assert!(!intent.begin(640, 480, || true));
    }
}
