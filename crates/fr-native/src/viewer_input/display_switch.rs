//! One local display-change decision, scoped to the original window and mode.
//! No catalog, target identity, connection, input authority or retry lives here.
use std::time::Duration;
#[cfg(any(feature = "linux-viewer-window", test))]
use std::{sync::Mutex, time::Instant};

#[cfg(any(feature = "linux-viewer-window", test))]
pub(crate) const MAX_AGE: Duration = Duration::from_millis(250);
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_millis(500);

#[cfg(any(feature = "linux-viewer-window", test))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Mode {
    #[default]
    Disabled = 0,
    Viewing = 1,
    Controlled = 2,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scope {
    #[cfg(any(feature = "linux-viewer-window", test))]
    pub(crate) mode: Mode,
    #[cfg(any(feature = "linux-viewer-window", test))]
    generation: u64,
}

#[cfg(any(feature = "linux-viewer-window", test))]
struct State {
    scope: Scope,
    requested: Option<Instant>,
    cancelled: bool,
}
#[cfg(any(feature = "linux-viewer-window", test))]
impl Default for State {
    fn default() -> Self {
        Self {
            scope: Scope {
                mode: Mode::Disabled,
                generation: 1,
            },
            requested: None,
            cancelled: false,
        }
    }
}
#[cfg(any(feature = "linux-viewer-window", test))]
#[derive(Default)]
pub(crate) struct Intent(Mutex<State>);
#[cfg(any(feature = "linux-viewer-window", test))]
impl Intent {
    pub(crate) fn set_mode(&self, mode: Mode) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled || state.requested.is_some() {
            return false;
        }
        if state.scope.mode == mode {
            return true;
        }
        let Some(generation) = state.scope.generation.checked_add(1) else {
            state.cancelled = true;
            return false;
        };
        state.scope = Scope { mode, generation };
        true
    }
    pub(crate) fn scope(&self) -> Option<Scope> {
        let state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (!state.cancelled && state.requested.is_none()).then_some(state.scope)
    }
    /// The original native timestamp includes queue and platform-query time.
    /// A mode change cannot turn a gesture captured in an earlier mode into intent.
    pub(crate) fn request(&self, scope: Scope, captured: Instant) -> bool {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.cancelled
            || state.requested.is_some()
            || scope.mode == Mode::Disabled
            || state.scope != scope
            || captured.elapsed() >= MAX_AGE
        {
            return false;
        }
        state.requested = Some(captured);
        true
    }
    pub(crate) fn requested(&self) -> Option<Instant> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .requested
    }
    pub(crate) fn invalidate(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.requested = None;
        match state.scope.generation.checked_add(1) {
            Some(next) => state.scope.generation = next,
            None => state.cancelled = true,
        }
    }
    /// Actual local close/escape or native failure wins over a pending switch.
    /// Ordinary cleanup retains the latch for the successful-close decision.
    pub(crate) fn cancel(&self) {
        let mut state = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.requested = None;
        state.cancelled = true;
    }
}

const NAMES: [[u8; 4]; 7] = [
    *b"LCTL", *b"RCTL", *b"LALT", *b"RALT", *b"LFSH", *b"RTSH", *b"FK07",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gesture {
    pub(crate) scope: Scope,
    pub(crate) captured: u64,
}
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) local: bool,
    pub(crate) request: Option<Gesture>,
}
/// Only the final keyboard event in a fully validated native batch may complete
/// a choice. A later press or release discards an earlier candidate even when
/// every key is released again before the batch ends.
#[derive(Default)]
pub(crate) struct BatchIntent(Option<Gesture>);
impl BatchIntent {
    pub(crate) fn key(&mut self, route: Route) {
        self.0 = route.request;
    }
    pub(crate) fn finish(self, shortcut: Option<&Shortcut>) -> Option<Gesture> {
        self.0
            .filter(|_| shortcut.is_some_and(|shortcut| !shortcut.has_held()))
    }
}
pub(crate) struct Shortcut {
    codes: [Option<u8>; 7],
    held: [bool; 256],
    local: bool,
    pending: Option<Gesture>,
    released: bool,
}
impl Shortcut {
    pub(crate) fn new(names: &[[u8; 4]; 256]) -> Option<Self> {
        let mut codes = [None; 7];
        for (slot, name) in NAMES.iter().enumerate() {
            for (code, actual) in names.iter().enumerate() {
                if actual == name {
                    if code < 8 || codes[slot].is_some() {
                        return None;
                    }
                    codes[slot] = Some(u8::try_from(code).ok()?);
                }
            }
        }
        if codes[6].is_none()
            || (0..3).any(|group| codes[2 * group..2 * group + 2].iter().all(Option::is_none))
        {
            return None;
        }
        Some(Self {
            codes,
            held: [false; 256],
            local: false,
            pending: None,
            released: false,
        })
    }
    /// Only call after whole-batch lifecycle and original timestamp validation.
    /// A function key pressed remotely before the chord retains that custody.
    /// Completing a chord retains its original F7-down timestamp and exact
    /// mode scope. Repeats and later releases cannot renew either one.
    pub(crate) fn key(
        &mut self,
        code: u32,
        down: bool,
        scope: Option<Scope>,
        captured: u64,
    ) -> Route {
        let Some(index) = usize::try_from(code)
            .ok()
            .filter(|code| (8..256).contains(code))
        else {
            self.pending = None;
            self.released = false;
            return Route::default();
        };
        let was_down = std::mem::replace(&mut self.held[index], down);
        let Some(slot) = self
            .codes
            .iter()
            .position(|key| key.is_some_and(|key| u32::from(key) == code))
        else {
            self.pending = None;
            self.released = false;
            return Route::default();
        };
        let mut route = Route::default();
        if slot == 6 {
            if down && !was_down {
                if self.pending.is_none() {
                    self.local = scope.is_some()
                        && (0..3).all(|group| {
                            self.codes[2 * group..2 * group + 2]
                                .iter()
                                .any(|code| code.is_some_and(|code| self.held[usize::from(code)]))
                        });
                    if self.local {
                        self.pending = scope.map(|scope| Gesture { scope, captured });
                    }
                } else {
                    self.local = true;
                }
                self.released = false;
            }
            route.local = self.local;
            if !down {
                self.released = self.local && self.pending.is_some();
                self.local = false;
            }
        }
        if self.released && !self.has_held() {
            self.released = false;
            route.request = self.pending.take();
        }
        route
    }
    /// Never adopt a physical press, and keep a local release local even if
    /// the server snapshot cleared the held bit before its event arrived.
    pub(crate) fn reconcile(&mut self, actual: &[u8; 32]) {
        for (code, held) in self.held.iter_mut().enumerate() {
            let was_held = *held;
            *held &= actual[code / 8] & (1 << (code % 8)) != 0;
            if was_held && !*held {
                // A lost release can reconcile remote held state, but cannot
                // complete or refresh a positive local command.
                self.pending = None;
                self.released = false;
            }
        }
    }
    pub(crate) fn has_held(&self) -> bool {
        self.held.iter().any(|held| *held)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> [[u8; 4]; 256] {
        let mut names = [[0; 4]; 256];
        for (slot, name) in NAMES.iter().enumerate() {
            names[slot + 8] = *name;
        }
        names
    }
    fn key(shortcut: &mut Shortcut, slot: u32, down: bool, enabled: bool) -> Route {
        shortcut.key(slot + 8, down, enabled.then_some(scope()), 10)
    }
    fn scope() -> Scope {
        Scope {
            mode: Mode::Controlled,
            generation: 1,
        }
    }
    fn local() -> Route {
        Route {
            local: true,
            request: None,
        }
    }
    #[test]
    fn intent_requires_the_original_enabled_scope_and_a_fresh_native_timestamp() {
        let intent = Intent::default();
        assert!(!intent.request(intent.scope().unwrap(), Instant::now()));
        assert!(intent.set_mode(Mode::Viewing));
        let old = intent.scope().unwrap();
        assert!(intent.set_mode(Mode::Disabled));
        assert!(intent.set_mode(Mode::Viewing));
        assert!(!intent.request(old, Instant::now()));
        let current = intent.scope().unwrap();
        assert!(!intent.request(current, Instant::now().checked_sub(MAX_AGE).unwrap()));
        assert!(intent.request(current, Instant::now()));
        let accepted = intent.requested();
        assert!(!intent.request(current, Instant::now()));
        assert!(!intent.set_mode(Mode::Controlled));
        assert_eq!(intent.requested(), accepted);
    }
    #[test]
    fn final_local_stop_erases_switch_intent_and_cannot_rearm_it() {
        let intent = Intent::default();
        assert!(intent.set_mode(Mode::Controlled));
        let scope = intent.scope().unwrap();
        assert!(intent.request(scope, Instant::now()));
        intent.cancel();
        assert!(intent.requested().is_none());
        assert!(!intent.set_mode(Mode::Viewing));
        assert!(!intent.request(scope, Instant::now()));
    }
    #[test]
    fn focus_or_mapping_invalidation_requires_a_new_scope_and_new_gesture() {
        let intent = Intent::default();
        assert!(intent.set_mode(Mode::Viewing));
        let old = intent.scope().unwrap();
        assert!(intent.request(old, Instant::now()));
        intent.invalidate();
        assert!(intent.requested().is_none());
        assert!(!intent.request(old, Instant::now()));
        let current = intent.scope().unwrap();
        assert!(intent.request(current, Instant::now()));
    }
    #[test]
    fn both_modifier_sides_keep_the_action_key_gesture_local_once() {
        for control in 0..=1 {
            for alt in 2..=3 {
                for shift in 4..=5 {
                    let mut shortcut = Shortcut::new(&names()).unwrap();
                    for slot in [control, alt, shift] {
                        assert_eq!(key(&mut shortcut, slot, true, true), Route::default());
                    }
                    assert_eq!(key(&mut shortcut, 6, true, true), local());
                    assert_eq!(key(&mut shortcut, 6, true, true), local());
                    assert_eq!(key(&mut shortcut, 6, false, false), local());
                    assert_eq!(key(&mut shortcut, control, false, false), Route::default());
                    assert_eq!(key(&mut shortcut, alt, false, false), Route::default());
                    assert_eq!(
                        key(&mut shortcut, shift, false, false),
                        Route {
                            local: false,
                            request: Some(Gesture {
                                scope: scope(),
                                captured: 10
                            }),
                        }
                    );
                }
            }
        }
    }
    #[test]
    fn a_remote_key_held_before_the_chord_or_enable_is_never_stolen() {
        for enabled in [false, true] {
            let mut shortcut = Shortcut::new(&names()).unwrap();
            assert_eq!(key(&mut shortcut, 6, true, enabled), Route::default());
            for slot in [0, 2, 4] {
                assert_eq!(key(&mut shortcut, slot, true, true), Route::default());
            }
            assert_eq!(key(&mut shortcut, 6, true, true), Route::default());
            assert_eq!(key(&mut shortcut, 6, false, true), Route::default());
        }
    }
    #[test]
    fn snapshots_only_release_and_missing_or_duplicate_maps_disable_the_shortcut() {
        let mut shortcut = Shortcut::new(&names()).unwrap();
        shortcut.reconcile(&[255; 32]);
        assert!(!shortcut.has_held());
        assert_eq!(key(&mut shortcut, 6, true, true), Route::default());
        let mut map = names();
        map[14] = [0; 4];
        assert!(Shortcut::new(&map).is_none());
        map[14] = NAMES[6];
        map[15] = NAMES[6];
        assert!(Shortcut::new(&map).is_none());
    }
    #[test]
    fn final_action_release_can_complete_but_held_repeat_never_renews_the_press() {
        let mut shortcut = Shortcut::new(&names()).unwrap();
        for slot in [0, 2, 4] {
            assert_eq!(key(&mut shortcut, slot, true, true), Route::default());
        }
        assert_eq!(shortcut.key(14, true, Some(scope()), 100), local());
        assert_eq!(shortcut.key(14, true, Some(scope()), 400_100), local());
        assert_eq!(shortcut.key(14, false, Some(scope()), 400_101), local());
        // Another F7 tap while modifiers remain held retains this same gesture.
        assert_eq!(shortcut.key(14, true, Some(scope()), 500_100), local());
        for slot in [0, 2, 4] {
            assert_eq!(key(&mut shortcut, slot, false, true), Route::default());
        }
        assert_eq!(
            shortcut.key(14, false, Some(scope()), 500_101),
            Route {
                local: true,
                request: Some(Gesture {
                    scope: scope(),
                    captured: 100
                }),
            }
        );
        assert_eq!(
            shortcut.key(14, false, Some(scope()), 500_102),
            Route::default()
        );
    }
    #[test]
    fn mode_changes_cannot_rebind_a_chord_completed_later() {
        let intent = Intent::default();
        assert!(intent.set_mode(Mode::Controlled));
        let original = intent.scope().unwrap();
        let mut shortcut = Shortcut::new(&names()).unwrap();
        for code in [8, 10, 12] {
            assert_eq!(
                shortcut.key(code, true, Some(original), 10),
                Route::default()
            );
        }
        assert_eq!(shortcut.key(14, true, Some(original), 20), local());
        assert!(intent.set_mode(Mode::Disabled));
        assert!(intent.set_mode(Mode::Controlled));
        let replacement = intent.scope();
        for code in [8, 10, 12] {
            assert_eq!(shortcut.key(code, false, replacement, 30), Route::default());
        }
        let completed = shortcut.key(14, false, replacement, 40);
        assert!(completed.local);
        assert_eq!(
            completed.request,
            Some(Gesture {
                scope: original,
                captured: 20
            })
        );
        assert!(!intent.request(completed.request.unwrap().scope, Instant::now()));
    }
    #[test]
    fn snapshot_release_cannot_complete_or_refresh_positive_intent() {
        let mut shortcut = Shortcut::new(&names()).unwrap();
        for slot in [0, 2, 4] {
            assert_eq!(key(&mut shortcut, slot, true, true), Route::default());
        }
        assert_eq!(key(&mut shortcut, 6, true, true), local());
        shortcut.reconcile(&[0; 32]);
        assert_eq!(key(&mut shortcut, 6, false, true), local());
        for slot in [0, 2, 4] {
            assert_eq!(key(&mut shortcut, slot, false, true), Route::default());
        }
    }
    #[test]
    fn unrelated_held_keys_refuse_completion_instead_of_being_carried_to_a_new_display() {
        let mut shortcut = Shortcut::new(&names()).unwrap();
        assert_eq!(shortcut.key(30, true, Some(scope()), 1), Route::default());
        for slot in [0, 2, 4] {
            assert_eq!(key(&mut shortcut, slot, true, true), Route::default());
        }
        assert_eq!(key(&mut shortcut, 6, true, true), local());
        assert_eq!(key(&mut shortcut, 6, false, true), local());
        for slot in [0, 2, 4] {
            assert_eq!(key(&mut shortcut, slot, false, true), Route::default());
        }
        assert_eq!(shortcut.key(30, false, Some(scope()), 30), Route::default());
        assert!(!shortcut.has_held());
    }
    fn completed_batch(shortcut: &mut Shortcut) -> BatchIntent {
        let mut batch = BatchIntent::default();
        for (code, down, at) in [
            (8, true, 10),
            (10, true, 11),
            (12, true, 12),
            (14, true, 13),
            (14, false, 14),
            (8, false, 15),
            (10, false, 16),
            (12, false, 17),
        ] {
            batch.key(shortcut.key(code, down, Some(scope()), at));
        }
        batch
    }
    #[test]
    fn later_keyboard_events_discard_completed_batch_intent_even_when_released_again() {
        let mut shortcut = Shortcut::new(&names()).unwrap();
        assert_eq!(
            completed_batch(&mut shortcut).finish(Some(&shortcut)),
            Some(Gesture {
                scope: scope(),
                captured: 13,
            })
        );
        for code in [8, 14, 30] {
            for release in [false, true] {
                let mut shortcut = Shortcut::new(&names()).unwrap();
                let mut batch = completed_batch(&mut shortcut);
                batch.key(shortcut.key(code, true, Some(scope()), 18));
                if release {
                    batch.key(shortcut.key(code, false, Some(scope()), 19));
                }
                assert_eq!(shortcut.has_held(), !release);
                assert_eq!(batch.finish(Some(&shortcut)), None);
            }
        }
    }
    #[test]
    fn final_batch_keyboard_state_must_still_prove_all_keys_released() {
        let mut shortcut = Shortcut::new(&names()).unwrap();
        let batch = completed_batch(&mut shortcut);
        // The final keyboard-state fence remains required independently of
        // candidate invalidation; retaining a candidate is not release proof.
        let _ = shortcut.key(30, true, Some(scope()), 18);
        assert_eq!(batch.finish(Some(&shortcut)), None);
        let mut shortcut = Shortcut::new(&names()).unwrap();
        assert_eq!(completed_batch(&mut shortcut).finish(None), None);
    }
    #[test]
    fn mode_generation_exhaustion_is_terminal() {
        let intent = Intent::default();
        intent.0.lock().unwrap().scope.generation = u64::MAX;
        assert!(!intent.set_mode(Mode::Viewing));
        assert!(intent.scope().is_none());
    }
}
