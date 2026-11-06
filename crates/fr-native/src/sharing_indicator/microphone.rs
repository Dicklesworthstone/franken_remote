//! Bounded local talk intent for the original client microphone owner. Only a
//! consumed, recent real-device gesture enables its predicate. Mute and every
//! terminal UI loss clear it independently of session/network-loop progress.
use super::{Error, IndicatorControl, LocalIndicator, Mode, Owner, StopReason, launch};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const MUTED: u8 = 0;
const TALK_PENDING: u8 = 1;
const TALK_ACCEPTED: u8 = 2;
const MUTE_PENDING: u8 = 3;
const TERMINAL: u8 = 4;
const GESTURE_LIFETIME: Duration = Duration::from_secs(2);

/// A local diagnostic label, not permission or proof that the remote host is
/// playing audio. Publish Live only while the actual capture owner is Talking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MicrophoneState {
    Muted = 0,
    Requesting = 1,
    Live = 2,
}

pub(super) struct Shared {
    intent: AtomicU8,
    label: AtomicU8,
    generation: AtomicU64,
    // A single original click time, never a queue or a refreshed permission.
    gesture: Mutex<Option<Instant>>,
}
impl Shared {
    pub(super) fn new() -> Self {
        Self {
            intent: AtomicU8::new(MUTED),
            label: AtomicU8::new(MicrophoneState::Muted as u8),
            generation: AtomicU64::new(0),
            gesture: Mutex::new(None),
        }
    }
    pub(super) fn label(&self) -> u8 {
        self.label.load(Ordering::Acquire)
    }
    pub(super) fn stop(&self) {
        self.intent.store(TERMINAL, Ordering::Release);
        self.label
            .store(MicrophoneState::Muted as u8, Ordering::Release);
    }
    pub(super) fn talk(&self) -> bool {
        let Ok(mut gesture) = self.gesture.lock() else {
            self.stop();
            return false;
        };
        // Repeated clicks cannot refresh an unconsumed original gesture.
        if self.intent.load(Ordering::Acquire) != MUTED
            || self.label.load(Ordering::Acquire) != MicrophoneState::Muted as u8
        {
            return true;
        }
        let Some(generation) = self.generation.load(Ordering::Acquire).checked_add(1) else {
            self.stop();
            return false;
        };
        // Advance at native gesture admission, before any later session poll.
        // A missed mute interval cannot make an old capture epoch current again.
        self.generation.store(generation, Ordering::Release);
        *gesture = Some(Instant::now());
        if self
            .intent
            .compare_exchange(MUTED, TALK_PENDING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.label
                .store(MicrophoneState::Requesting as u8, Ordering::Release);
        }
        true
    }
    pub(super) fn mute(&self) {
        // Clear accepted talk BEFORE the coordinator can consume a request.
        // A concurrent terminal stop is never overwritten by a mute.
        let _ = self
            .intent
            .try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                (state != TERMINAL).then_some(MUTE_PENDING)
            });
        self.label
            .store(MicrophoneState::Muted as u8, Ordering::Release);
    }
    fn take_talk(&self, now: Instant) -> bool {
        if self.intent.load(Ordering::Acquire) != TALK_PENDING {
            return false;
        }
        let Ok(gesture) = self.gesture.lock() else {
            self.stop();
            return false;
        };
        if self.intent.load(Ordering::Acquire) != TALK_PENDING {
            return false;
        }
        if gesture.is_none_or(|issued| {
            now.checked_duration_since(issued)
                .is_none_or(|age| age >= GESTURE_LIFETIME)
        }) {
            let _ = self.intent.compare_exchange(
                TALK_PENDING,
                MUTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            self.label
                .store(MicrophoneState::Muted as u8, Ordering::Release);
            return false;
        }
        self.intent
            .compare_exchange(
                TALK_PENDING,
                TALK_ACCEPTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
    fn publish(&self, state: MicrophoneState) {
        if self.intent.load(Ordering::Acquire) == TERMINAL {
            return;
        }
        if state == MicrophoneState::Muted {
            // Keep an unconsumed fresh gesture: it happened after the caller's
            // last runtime observation, so its diagnostic update cannot eat it.
            let _ = self.intent.compare_exchange(
                TALK_ACCEPTED,
                MUTED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        self.label.store(state as u8, Ordering::Release);
    }
}

/// Visible per-session control. It starts muted; mapping, status publication,
/// operating-system permission and reconnection never produce a talk gesture.
#[must_use]
pub struct MicrophonePanel {
    indicator: LocalIndicator,
}
impl MicrophonePanel {
    pub fn start(
        display: &str,
        on_revoke: impl Fn(StopReason) + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        Self::open(display, None, on_revoke)
    }
    /// Keep the exact original local viewer unobscured. The native boundary
    /// chooses a separate bounded rectangle or refuses; it never moves/resizes
    /// the viewer. Reparenting, dimensions and separation remain checked.
    pub fn start_for_viewer(
        display: &str,
        target: fr_media::worker::presentation::X11Target,
        on_revoke: impl Fn(StopReason) + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        Self::open(display, Some(target), on_revoke)
    }
    fn open(
        display: &str,
        target: Option<fr_media::worker::presentation::X11Target>,
        on_revoke: impl Fn(StopReason) + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        let (control, task) = launch(
            display,
            Owner::Local(Box::new(on_revoke)),
            Mode::MicrophoneClient(target),
        )?;
        Ok(Self {
            indicator: LocalIndicator {
                control,
                task: Some(task),
            },
        })
    }
    pub fn control(&self) -> IndicatorControl {
        self.indicator.control()
    }
    pub fn finish(&mut self) -> Option<StopReason> {
        self.indicator.finish()
    }
    fn shared(&self) -> &Shared {
        self.indicator
            .control
            .0
            .microphone
            .as_ref()
            .expect("microphone panel")
    }
    /// Consume at most one recent explicit gesture. Mute wins a racing Talk;
    /// this is a local request, so original microphone authority must be checked
    /// again by the caller before creating a new talk generation.
    pub fn take_talk_request(&self) -> bool {
        let control = &self.indicator.control;
        if !control.responsive() || !self.shared().take_talk(Instant::now()) {
            return false;
        }
        control.responsive() && self.shared().intent.load(Ordering::Acquire) == TALK_ACCEPTED
    }
    pub fn take_mute_request(&self) -> bool {
        self.shared()
            .intent
            .compare_exchange(MUTE_PENDING, MUTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
    /// Purely diagnostic. Publishing Muted retires an accepted intent; no
    /// other publication creates or restores one. Drain gestures first.
    pub fn publish(&self, state: MicrophoneState) -> bool {
        if !self.indicator.control.responsive() {
            return false;
        }
        self.shared().publish(state);
        self.indicator.control.responsive()
    }
    /// Current local intent for diagnostics. Actual capture/packet epochs must
    /// compare `talk_epoch` tokens so they cannot miss a fast mute/retalk.
    pub fn talk_check(&self) -> Arc<dyn Fn() -> bool + Send + Sync> {
        let epoch = self.talk_epoch();
        Arc::new(move || epoch().is_some())
    }
    /// The original accepted gesture generation, or None while muted, pending,
    /// hidden or stopped. A capture/packet epoch must retain its exact nonzero
    /// token and compare it on every check. Observing an intervening false is
    /// unnecessary: even a fast mute/retalk always has a different token.
    pub fn talk_epoch(&self) -> Arc<dyn Fn() -> Option<u64> + Send + Sync> {
        let control = self.control();
        Arc::new(move || {
            if !control.responsive() {
                return None;
            }
            let shared = control.0.microphone.as_ref()?;
            let generation = shared.generation.load(Ordering::Acquire);
            (generation != 0
                && shared.intent.load(Ordering::Acquire) == TALK_ACCEPTED
                && shared.generation.load(Ordering::Acquire) == generation)
                .then_some(generation)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{PROGRESS_TIMEOUT, Status};
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    fn panel() -> (MicrophonePanel, Arc<AtomicBool>) {
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        let control = IndicatorControl(Arc::new(super::super::Shared {
            owner: Owner::Local(Box::new(move |_| flag.store(true, Ordering::Release))),
            state: AtomicU8::new(1),
            window: AtomicU32::new(1),
            last_turn: Mutex::new(Instant::now()),
            microphone: Some(Shared::new()),
        }));
        (
            MicrophonePanel {
                indicator: LocalIndicator {
                    control,
                    task: None,
                },
            },
            stopped,
        )
    }

    #[test]
    fn mapping_or_status_publication_never_activates_a_microphone() {
        let shared = Shared::new();
        for state in [
            MicrophoneState::Muted,
            MicrophoneState::Requesting,
            MicrophoneState::Live,
        ] {
            shared.publish(state);
            assert!(!shared.take_talk(Instant::now()));
            assert_eq!(shared.intent.load(Ordering::Acquire), MUTED);
        }
    }

    #[test]
    fn one_recent_gesture_is_consumed_once_and_mute_clears_it_first() {
        let shared = Shared::new();
        shared.talk();
        assert_eq!(shared.intent.load(Ordering::Acquire), TALK_PENDING);
        shared.publish(MicrophoneState::Muted);
        assert!(shared.take_talk(Instant::now()));
        assert!(!shared.take_talk(Instant::now()));
        assert_eq!(shared.intent.load(Ordering::Acquire), TALK_ACCEPTED);
        shared.mute();
        assert_eq!(shared.intent.load(Ordering::Acquire), MUTE_PENDING);
        assert!(!shared.take_talk(Instant::now()));
        shared.talk();
        assert_eq!(shared.intent.load(Ordering::Acquire), MUTE_PENDING);
    }

    #[test]
    fn repeated_gestures_cannot_refresh_the_pending_request_deadline() {
        let shared = Shared::new();
        shared.talk();
        let original = shared.gesture.lock().unwrap().unwrap();
        shared.talk();
        assert_eq!(*shared.gesture.lock().unwrap(), Some(original));
        assert!(!shared.take_talk(original + GESTURE_LIFETIME));
        assert_eq!(shared.intent.load(Ordering::Acquire), MUTED);
    }

    #[test]
    fn terminal_loss_cannot_be_reopened_by_late_gestures_or_status() {
        for accepted in [false, true] {
            let shared = Shared::new();
            shared.talk();
            if accepted {
                assert!(shared.take_talk(Instant::now()));
            }
            shared.stop();
            shared.mute();
            shared.talk();
            shared.publish(MicrophoneState::Live);
            assert!(!shared.take_talk(Instant::now()));
            assert_eq!(shared.intent.load(Ordering::Acquire), TERMINAL);
        }
    }

    #[test]
    fn independent_capture_check_expires_before_the_session_drains_mute() {
        let (panel, stopped) = panel();
        let check = panel.talk_check();
        assert!(!check());
        panel.shared().talk();
        assert!(
            !check(),
            "a queued click has not been accepted by the session"
        );
        assert!(panel.take_talk_request());
        assert!(check());
        panel.shared().mute();
        assert!(!check(), "mute must fence before the session loop resumes");
        assert!(panel.take_mute_request());
        assert!(!panel.take_mute_request());
        assert!(!check());
        assert!(
            !stopped.load(Ordering::Acquire),
            "mute keeps the attachment available"
        );
        panel.shared().talk();
        assert!(panel.take_talk_request());
        assert!(check());
        drop(panel);
        assert!(stopped.load(Ordering::Acquire));
        assert!(
            !check(),
            "the retained guard cannot survive its original UI owner"
        );
    }

    #[test]
    fn ui_worker_stall_fences_accepted_talk_without_coordinator_progress() {
        let (panel, stopped) = panel();
        panel.shared().talk();
        assert!(panel.take_talk_request());
        let check = panel.talk_check();
        *panel.control().0.last_turn.lock().unwrap() = Instant::now() - PROGRESS_TIMEOUT;
        assert!(!check());
        assert!(stopped.load(Ordering::Acquire));
        assert_eq!(
            panel.control().status(),
            Status::Stopped(StopReason::NativeFailure)
        );
        *panel.control().0.last_turn.lock().unwrap() = Instant::now();
        assert!(!check());
        assert!(!panel.take_talk_request());
    }

    #[test]
    fn fast_mute_retalk_has_a_new_token_without_an_intermediate_capture_check() {
        let (panel, _) = panel();
        let epoch = panel.talk_epoch();
        assert_eq!(epoch(), None);
        assert!(panel.shared().talk());
        assert_eq!(panel.shared().generation.load(Ordering::Acquire), 1);
        assert_eq!(epoch(), None, "native gesture alone is not accepted talk");
        assert!(panel.take_talk_request());
        let old = epoch().unwrap();
        panel.shared().mute();
        assert!(panel.take_mute_request());
        assert!(panel.shared().talk());
        assert!(panel.take_talk_request());
        assert_ne!(
            epoch(),
            Some(old),
            "old capture must not need to see a false interval"
        );
        assert_eq!(epoch(), Some(2));
    }

    #[test]
    fn talk_generation_cannot_wrap_or_repeat_after_exhaustion() {
        let shared = Shared::new();
        shared.generation.store(u64::MAX, Ordering::Release);
        assert!(!shared.talk());
        assert_eq!(shared.intent.load(Ordering::Acquire), TERMINAL);
        assert!(!shared.take_talk(Instant::now()));
    }
}
