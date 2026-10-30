//! Finite read-only observation uses the same viewer close and reap path.
use fr_wire::{closure::Reason, display::Display};
use frd::{native_connection::reconnect::CallbackError, session_startup::Presentation};
use std::ops::ControlFlow;

/// The selected display and actual initial decoder completion of ONE attempt.
/// No screenshot, host clock mapping, semantic result or reusable grant is implied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observation {
    pub display: Display,
    pub completion: Presentation,
}

pub(super) fn flow(
    one_shot: bool,
    continuous: impl FnOnce() -> Result<(), CallbackError>,
) -> Result<ControlFlow<Reason>, CallbackError> {
    if one_shot {
        // Do not wait for a second frame: an unchanged desktop may stay idle.
        // Break asks the original viewer to close; it does NOT skip cleanup.
        Ok(ControlFlow::Break(Reason::InspectionComplete))
    } else {
        continuous().map(|()| ControlFlow::Continue(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop::{
        Configuration,
        reconnect::{Mode, Session},
    };
    use frd::session_startup::ObserverPolicy;
    use std::{cell::Cell, path::Path};

    #[test]
    fn inspection_finishes_without_waiting_for_another_frame_or_calling_live_ui() {
        let called = Cell::new(false);
        let result = flow(true, || {
            called.set(true);
            Err(CallbackError)
        });
        assert!(matches!(
            result,
            Ok(ControlFlow::Break(Reason::InspectionComplete))
        ));
        assert!(!called.get());
    }

    #[test]
    fn continuous_sessions_still_call_their_ui_and_propagate_refusals() {
        let called = Cell::new(false);
        assert!(matches!(
            flow(false, || {
                called.set(true);
                Ok(())
            }),
            Ok(ControlFlow::Continue(()))
        ));
        assert!(called.get());
        assert!(flow(false, || Err(CallbackError)).is_err());
    }

    #[test]
    fn one_shot_constructor_selects_only_observation_and_starts_without_evidence() {
        let configuration =
            Configuration::new(Path::new("/local/fr-media-worker"), ":0", None, 1).unwrap();
        let session = Session::observing_once(configuration, ObserverPolicy::default(), ());
        assert!(matches!(session.mode, Mode::Observe));
        assert!(session.one_shot);
        assert!(session.initial_observation().is_none());
        assert!(session.desktop().is_none());
    }
}
