//! Bounded native upload gestures, pinned to the batch actually displayed.
//! This owns no transfer, deadline or authority. The retained handle is the
//! original sender's control; a later batch cannot inherit an earlier click.
#![forbid(unsafe_code)]

use std::time::Duration;

pub(crate) const CANCEL_UPLOAD: u32 = 1;
pub(crate) const CANCEL_DOWNLOAD: u32 = 2;
pub(crate) const PAUSE_UPLOAD: u32 = 4;
pub(crate) const RESUME_UPLOAD: u32 = 8;
pub(crate) const ALL_ACTIONS: u32 = 15;
const MAX_AGE: Duration = Duration::from_millis(250);

/// A request and the last sender observation are separate facts. In
/// particular, choosing Resume does not turn an observed pause into Running.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum State {
    #[default]
    Unavailable = 0,
    Running = 1,
    RunPending = 2,
    Paused = 3,
    PausePending = 4,
}
impl State {
    pub(crate) const fn new(eligible: bool, requested: bool, observed: Option<bool>) -> Self {
        if !eligible {
            return Self::Unavailable;
        }
        match (requested, observed) {
            (false, Some(false)) => Self::Running,
            (false, None | Some(true)) => Self::RunPending,
            (true, Some(true)) => Self::Paused,
            (true, None | Some(false)) => Self::PausePending,
        }
    }

    pub(crate) const fn permits(self, action: Action) -> bool {
        matches!(
            (self, action),
            (Self::Running | Self::RunPending, Action::Pause)
                | (Self::Paused | Self::PausePending, Action::Resume)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Pause,
    Resume,
}

/// One content-free UI identity and the exact original handle behind it.
/// `show` runs only after the native update succeeded. No application worker
/// can substitute the target between drawing the button and consuming it.
pub(crate) struct Displayed<T> {
    owner: Option<(u64, T, State)>,
}
impl<T> Default for Displayed<T> {
    fn default() -> Self {
        Self { owner: None }
    }
}
impl<T> Displayed<T> {
    pub(crate) fn show(&mut self, context: u64, owner: Option<T>, state: State) {
        self.owner = owner
            .filter(|_| context != 0 && state != State::Unavailable)
            .map(|owner| (context, owner, state));
    }

    /// Resolve against the displayed owner, never a newly sampled "latest"
    /// batch. The caller must also recheck this handle's current eligibility
    /// before issuing its ordinary pause/resume request.
    pub(crate) fn resolve(
        &self,
        actions: u32,
        context: u64,
        captured: Option<Duration>,
        now: Duration,
    ) -> Result<Option<(Action, &T)>, ()> {
        if actions & !ALL_ACTIONS != 0 {
            return Err(());
        }
        if actions & CANCEL_UPLOAD != 0 {
            return Ok(None);
        }
        let action = match actions & (PAUSE_UPLOAD | RESUME_UPLOAD) {
            0 if context == 0 && captured.is_none() => return Ok(None),
            PAUSE_UPLOAD => Action::Pause,
            RESUME_UPLOAD => Action::Resume,
            _ => return Err(()),
        };
        let captured = captured.filter(|_| context != 0).ok_or(())?;
        if now.checked_sub(captured).is_none_or(|age| age >= MAX_AGE) {
            return Ok(None);
        }
        let Some((displayed, owner, state)) = &self.owner else {
            return Ok(None);
        };
        Ok((*displayed == context && state.permits(action)).then_some((action, owner)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn requests_never_fabricate_sender_observation() {
        for observed in [None, Some(false), Some(true)] {
            assert_eq!(State::new(false, false, observed), State::Unavailable);
            assert_eq!(State::new(false, true, observed), State::Unavailable);
        }
        assert_eq!(State::new(true, false, None), State::RunPending);
        assert_eq!(State::new(true, false, Some(false)), State::Running);
        assert_eq!(State::new(true, false, Some(true)), State::RunPending);
        assert_eq!(State::new(true, true, None), State::PausePending);
        assert_eq!(State::new(true, true, Some(false)), State::PausePending);
        assert_eq!(State::new(true, true, Some(true)), State::Paused);
    }

    #[test]
    fn an_old_button_never_targets_a_newly_displayed_batch() {
        let mut displayed = Displayed::default();
        displayed.show(1, Some("original"), State::Paused);
        assert_eq!(
            displayed.resolve(RESUME_UPLOAD, 1, Some(at(10)), at(20)),
            Ok(Some((Action::Resume, &"original")))
        );
        displayed.show(2, Some("successor"), State::Paused);
        assert_eq!(
            displayed.resolve(RESUME_UPLOAD, 1, Some(at(10)), at(20)),
            Ok(None)
        );
        assert_eq!(
            displayed.resolve(RESUME_UPLOAD, 2, Some(at(20)), at(21)),
            Ok(Some((Action::Resume, &"successor")))
        );
    }

    #[test]
    fn the_displayed_handle_survives_external_latest_selection_changes() {
        // A selector can publish a new current batch while AppKit is waiting.
        // Only a successful native redraw may replace this retained handle.
        let mut displayed = Displayed::default();
        displayed.show(7, Some(41), State::Running);
        let newly_selected = 42;
        assert_eq!(
            displayed.resolve(PAUSE_UPLOAD, 7, Some(at(10)), at(11)),
            Ok(Some((Action::Pause, &41)))
        );
        displayed.show(8, Some(newly_selected), State::Running);
        assert_eq!(
            displayed.resolve(PAUSE_UPLOAD, 8, Some(at(12)), at(13)),
            Ok(Some((Action::Pause, &42)))
        );
    }

    #[test]
    fn exact_age_future_and_retired_gestures_do_not_resume() {
        let mut displayed = Displayed::default();
        displayed.show(1, Some(1), State::Paused);
        assert!(
            displayed
                .resolve(RESUME_UPLOAD, 1, Some(at(10)), at(259))
                .unwrap()
                .is_some()
        );
        for now in [at(9), at(260), at(10_000)] {
            assert_eq!(
                displayed.resolve(RESUME_UPLOAD, 1, Some(at(10)), now),
                Ok(None)
            );
        }
        displayed.show(1, Some(1), State::Unavailable);
        assert_eq!(
            displayed.resolve(RESUME_UPLOAD, 1, Some(at(10)), at(11)),
            Ok(None)
        );
        displayed.show(0, Some(1), State::Paused);
        assert_eq!(
            displayed.resolve(RESUME_UPLOAD, 1, Some(at(10)), at(11)),
            Ok(None)
        );
    }

    #[test]
    fn cancellation_wins_and_download_cancellation_cannot_change_the_upload_target() {
        let mut displayed = Displayed::default();
        displayed.show(1, Some(1), State::Paused);
        assert_eq!(
            displayed.resolve(CANCEL_UPLOAD | RESUME_UPLOAD, 1, Some(at(10)), at(11)),
            Ok(None)
        );
        assert_eq!(
            displayed.resolve(CANCEL_DOWNLOAD | RESUME_UPLOAD, 1, Some(at(10)), at(11)),
            Ok(Some((Action::Resume, &1)))
        );
        assert_eq!(
            displayed.resolve(CANCEL_DOWNLOAD, 0, None, at(11)),
            Ok(None)
        );
    }

    #[test]
    fn malformed_and_disabled_actions_never_reach_a_handle() {
        let mut displayed = Displayed::default();
        displayed.show(1, Some(1), State::Running);
        assert_eq!(
            displayed.resolve(RESUME_UPLOAD, 1, Some(at(10)), at(11)),
            Ok(None)
        );
        for (actions, context, captured) in [
            (16, 0, None),
            (PAUSE_UPLOAD | RESUME_UPLOAD, 1, Some(at(10))),
            (PAUSE_UPLOAD, 0, Some(at(10))),
            (PAUSE_UPLOAD, 1, None),
            (0, 1, None),
            (0, 0, Some(at(10))),
        ] {
            assert_eq!(
                displayed.resolve(actions, context, captured, at(11)),
                Err(())
            );
        }
        displayed.show(1, None, State::Running);
        assert_eq!(
            displayed.resolve(PAUSE_UPLOAD, 1, Some(at(10)), at(11)),
            Ok(None)
        );
    }
}
