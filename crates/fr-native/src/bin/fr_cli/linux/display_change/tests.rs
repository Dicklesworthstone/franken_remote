//! Policy/report tests; actual local gestures are covered at the native owner.
use super::*;
use crate::linux::{DisplayChoice, Interface, Ui, control};
use fr_native::desktop::reconnect::CleanupSummary;
use frd::session_startup::ViewerCloseOutcome;
use std::{cell::RefCell, rc::Rc};

fn progress() -> Progress {
    let mut progress = Progress {
        control: Some(control::Counters::default()),
        ..Progress::default()
    };
    progress.begin(1);
    progress.cleanup = Some(CleanupSummary {
        disconnect: Some(ViewerCloseOutcome {
            request_acknowledged: false,
            report: None,
            transport: Err(fr_transport::quic::Error::Expired),
        }),
        control_disconnect: None,
        input_stop: None,
        failure: None,
    });
    progress
}

#[test]
fn switching_before_control_retires_unused_consent_without_inventing_a_request() {
    let mut progress = progress();
    record(&mut progress, 2).unwrap();
    let original = progress.control.unwrap();
    assert!(!original.requested);
    assert!(!original.granted);
    assert_eq!(original.request_attempt, None);
    assert_eq!(original.ended, None);
    assert_eq!(original.initial_decision_retired_at, Some(1));
    assert!(!original.initial_decision_available());
    progress.begin(2);
    let control = progress.control.as_mut().unwrap();
    assert_eq!(control.opened(), control::AttemptMode::ViewingOnly);
    control.spend_fresh_request().unwrap();
    assert_eq!(control.mode, control::AttemptMode::ExplicitControl);
    assert!(!control.requested);
    assert_eq!(control.request_attempt, None);
    assert!(control.fresh[1].is_some());
    assert_eq!(control.spend_fresh_request(), Err(CallbackError));
    progress.begin(3);
    assert_eq!(
        progress.control.unwrap().mode,
        control::AttemptMode::ViewingOnly
    );
}

#[test]
fn retired_initial_choice_never_reoffers_original_file_consent() {
    let mut progress = progress();
    record(&mut progress, 2).unwrap();
    progress.begin(2);
    let original = fr_client::native::control_offer_with_files();
    let mut ui = Interface {
        display: DisplayChoice::Choose,
        progress: Rc::new(RefCell::new(progress)),
        attempt: None,
        clipboard: None,
        audio: None,
    };
    let offer = ui.offer(2, &original).unwrap();
    assert!(offer.validate().is_ok());
    assert!(offer.capabilities.iter().all(|capability| !matches!(
        capability.name.as_str(),
        fr_wire::attachment::FILES_CAPABILITY
            | fr_wire::files::CAPABILITY
            | fr_wire::files::CHANNEL_SCOPE_CAPABILITY
            | fr_wire::files::DOWNLOAD_CAPABILITY
            | fr_wire::files::DUPLEX_CAPABILITY
    )));
    for required in original
        .capabilities
        .iter()
        .filter(|capability| capability.required)
    {
        assert!(offer.capabilities.contains(required));
    }
}

#[test]
fn fresh_request_requires_a_later_attempt_even_when_initial_request_was_unsent() {
    let mut progress = progress();
    record(&mut progress, 2).unwrap();
    let counters = progress.control.as_mut().unwrap();
    counters.begin_attempt(1);
    assert_eq!(counters.spend_fresh_request(), Err(CallbackError));
    counters.begin_attempt(2);
    assert!(counters.spend_fresh_request().is_ok());
    assert_eq!(counters.initial_decision_retired_at, Some(1));
    counters.begin_attempt(33);
    assert_eq!(counters.spend_fresh_request(), Err(CallbackError));
}

#[test]
fn explicit_change_does_not_erase_original_control_or_unknown_closing_evidence() {
    let mut progress = progress();
    let counters = progress.control.as_mut().unwrap();
    counters.spend_request();
    counters.granted = true;
    counters.submitted = 9;
    counters.cleaning(None);
    record(&mut progress, 2).unwrap();
    progress.begin(2);
    let counters = progress.control.unwrap();
    assert_eq!(counters.request_attempt, Some(1));
    assert_eq!(counters.ended, Some(Ok(())));
    assert_eq!(counters.submitted, 9);
    let rendered = history(&progress);
    let value: serde_json::Value =
        serde_json::from_str(&format!("{{\"fixture\":true{}}}", rendered.json)).unwrap();
    assert_eq!(value["display_changes"], 1);
    assert_eq!(value["display_change_closes"][0]["attempt"], 1);
    assert_eq!(
        value["display_change_closes"][0]["lease_report"],
        serde_json::Value::Null
    );
    assert!(rendered.text.contains("unknown"));
}

#[test]
fn failed_cleanup_real_close_duplicate_and_out_of_order_changes_do_not_retire_consent() {
    for next in [0, 1, 3, 33] {
        let mut progress = progress();
        assert_eq!(record(&mut progress, next), Err(CallbackError));
        assert!(progress.control.unwrap().initial_decision_available());
        assert!(progress.display_change_closes.is_empty());
    }
    let mut closed = progress();
    closed.user_closed = true;
    assert_eq!(record(&mut closed, 2), Err(CallbackError));
    let mut failed = progress();
    failed.cleanup.as_mut().unwrap().failure =
        Some(fr_native::desktop::reconnect::CleanupFailure::Media);
    assert_eq!(record(&mut failed, 2), Err(CallbackError));
    let mut missing = progress();
    missing.cleanup = None;
    assert_eq!(record(&mut missing, 2), Err(CallbackError));
    let mut duplicate = progress();
    record(&mut duplicate, 2).unwrap();
    assert_eq!(record(&mut duplicate, 2), Err(CallbackError));
    assert_eq!(duplicate.display_changes, 1);
}

fn switched_control() -> Progress {
    use fr_wire::{
        closure::{Cleanup, Closed, ClosedReason, OutstandingEffects},
        lease_revoked::{CleanupStage, EffectStage, Reason, Revoked},
    };
    let mut progress = progress();
    let summary = CleanupSummary {
        disconnect: None,
        control_disconnect: Some(frd::session_startup::ControlCloseOutcome {
            exchange: ViewerCloseOutcome {
                request_acknowledged: true,
                report: Some(Closed {
                    reason: ClosedReason::ClientRequested,
                    cleanup: Cleanup::Complete,
                    effects: OutstandingEffects::Known {
                        pending: 0,
                        uncertain: 2,
                    },
                }),
                transport: Ok(()),
            },
            revocation: Some(Revoked {
                lease: fr_core::ids::InputLeaseId::from_raw(81),
                reason: Reason::ClientRequested,
                cleanup: CleanupStage::Released,
                effects: EffectStage::ReceiptsPending,
            }),
        }),
        input_stop: Some(fr_native::viewer_input::StopReason::Local),
        failure: None,
    };
    let counters = progress.control.as_mut().unwrap();
    counters.spend_request();
    counters.granted = true;
    counters.retain_cleanup(1, summary);
    counters.cleaning(None);
    assert!(
        counters.retry_stopped(),
        "automatic retry cannot reinterpret the close"
    );
    progress.cleanup = Some(summary);
    record(&mut progress, 2).unwrap();
    progress.begin(2);
    progress
}

fn interface(progress: Progress) -> Interface {
    Interface {
        display: DisplayChoice::Choose,
        progress: Rc::new(RefCell::new(progress)),
        attempt: None,
        clipboard: None,
        audio: None,
    }
}

#[test]
fn a_successful_display_change_does_not_block_later_viewing_recovery() {
    use frd::native_connection::reconnect::{Failure, Status};
    let mut ui = interface(switched_control());
    let transient = Failure::Observation(frd::session_startup::ObserverError::Streaming(
        frd::session_startup::StreamingViewerError::Transport(fr_transport::quic::Error::Expired),
    ));
    ui.status(Status::Cleaning {
        attempt: 2,
        failure: Some(transient),
    })
    .unwrap();
    ui.cleanup_finished(
        2,
        CleanupSummary {
            disconnect: None,
            control_disconnect: None,
            input_stop: None,
            failure: None,
        },
    );
    let reconnecting = Status::Reconnecting {
        attempt: 3,
        delay: std::time::Duration::from_millis(250),
        failure: transient,
    };
    assert_eq!(ui.status(reconnecting), Ok(()));
    {
        let progress = ui.progress.borrow();
        let control = progress.control.unwrap();
        assert!(
            control
                .latest_cleanup()
                .unwrap()
                .control_disconnect
                .is_some()
        );
        assert!(control.current_cleanup().is_none());
        assert!(!control.retry_stopped());
        assert_eq!(progress.display_change_closes.len(), 1);
    }
    // A stop belonging to the new viewing attempt still vetoes that attempt.
    ui.cleanup_finished(
        2,
        CleanupSummary {
            disconnect: None,
            control_disconnect: None,
            input_stop: Some(fr_native::viewer_input::StopReason::FocusLost),
            failure: None,
        },
    );
    assert_eq!(ui.status(reconnecting), Err(CallbackError));
}

#[test]
fn a_fresh_control_owner_still_vetoes_its_own_failed_cleanup_report() {
    let mut progress = switched_control();
    let control = progress.control.as_mut().unwrap();
    control.spend_fresh_request().unwrap();
    let current = CleanupSummary {
        disconnect: None,
        control_disconnect: None,
        input_stop: Some(fr_native::viewer_input::StopReason::LocalEscape),
        failure: None,
    };
    control.retain_cleanup(2, current);
    assert_eq!(control.current_cleanup(), Some(current));
    assert!(control.retry_stopped());
    control.begin_attempt(3);
    assert!(control.current_cleanup().is_none());
    assert_eq!(control.latest_cleanup(), Some(current));
}

#[test]
fn a_later_notification_failure_is_not_named_after_the_prior_successful_close() {
    use crate::linux::{DesktopConfiguration, Mode, ObserverPolicy, Session};
    let progress = switched_control();
    let session = Session::new(
        DesktopConfiguration::new(std::path::Path::new("/usr/bin/false"), ":0", None, 1).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        interface(Progress::default()),
    );
    let failed = crate::linux::completed(
        &session,
        Err(frd::native_connection::reconnect::Failure::Notification),
        false,
        &progress,
        true,
    )
    .unwrap_err();
    assert_eq!(failed.code, "native_session_failed");
    assert_eq!(progress.control.unwrap().request_attempt, Some(1));
    assert!(history(&progress).text.contains("client_requested"));
}

#[test]
fn local_resize_retires_original_consent_and_reports_its_own_viewport_history() {
    let mut progress = progress();
    progress.cleanup.as_mut().unwrap().input_stop =
        Some(fr_native::viewer_input::StopReason::WindowResized);
    record_resize(&mut progress, 2, (960, 540)).unwrap();
    assert_eq!(progress.display_changes, 0);
    assert!(progress.display_change_closes.is_empty());
    assert_eq!(progress.window_resizes.len(), 1);
    assert!(!progress.control.unwrap().initial_decision_available());
    assert!(!progress.control.unwrap().requested);
    progress.begin(2);
    assert_eq!(
        progress.control.as_mut().unwrap().opened(),
        control::AttemptMode::ViewingOnly
    );
    let report = history(&progress);
    let json: serde_json::Value =
        serde_json::from_str(&format!("{{\"fixture\":true{}}}", report.json)).unwrap();
    assert_eq!(json["display_changes"], 0);
    assert_eq!(json["window_resizes"][0]["attempt"], 1);
    assert_eq!(json["window_resizes"][0]["width"], 960);
    assert_eq!(json["window_resizes"][0]["height"], 540);
    assert!(!report.text.contains("Display change"));
    assert!(report.text.contains("host cleanup remains unknown"));
    let mut ui = interface(progress);
    let offer = ui
        .offer(2, &fr_client::native::control_offer_with_files())
        .unwrap();
    assert!(offer.capabilities.iter().all(|capability| !matches!(
        capability.name.as_str(),
        fr_wire::attachment::FILES_CAPABILITY
            | fr_wire::files::CAPABILITY
            | fr_wire::files::CHANNEL_SCOPE_CAPABILITY
    )));
}

#[test]
fn resize_only_observer_history_survives_a_later_failed_dial() {
    use crate::linux::{DesktopConfiguration, Mode, ObserverPolicy, Session};
    let mut progress = progress();
    progress.control = None;
    progress.ended.push(1, "native_session_failed");
    record_resize(&mut progress, 2, (640, 480)).unwrap();
    progress.begin(2);
    let session = Session::new(
        DesktopConfiguration::new(std::path::Path::new("/usr/bin/false"), ":0", None, 1).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        interface(Progress::default()),
    );
    let failed = crate::linux::completed(
        &session,
        Err(frd::native_connection::reconnect::Failure::Notification),
        false,
        &progress,
        true,
    )
    .unwrap_err();
    assert_eq!(failed.code, "native_session_failed");
    let retained = failed
        .control_history
        .expect("resize history was discarded");
    assert!(retained.json.contains("\"window_resizes\""));
    assert!(retained.json.contains("\"attempt\":1"));
    assert!(retained.text.contains("Window resize"));
}

#[test]
fn resize_bounds_duplicate_intents_and_adverse_cleanup_cannot_spend_consent() {
    for size in [(0, 480), (641, 480), (640, 481), (8192, 8192)] {
        let mut value = progress();
        assert_eq!(record_resize(&mut value, 2, size), Err(CallbackError));
        assert!(value.control.unwrap().initial_decision_available());
        assert!(value.window_resizes.is_empty());
    }
    for reason in [
        fr_native::viewer_input::StopReason::FocusLost,
        fr_native::viewer_input::StopReason::WindowChanged,
        fr_native::viewer_input::StopReason::LocalEscape,
    ] {
        let mut value = progress();
        value.cleanup.as_mut().unwrap().input_stop = Some(reason);
        assert_eq!(record_resize(&mut value, 2, (640, 480)), Err(CallbackError));
        assert!(value.control.unwrap().initial_decision_available());
    }
    let mut failed = progress();
    failed.cleanup.as_mut().unwrap().failure =
        Some(fr_native::desktop::reconnect::CleanupFailure::Media);
    assert_eq!(
        record_resize(&mut failed, 2, (640, 480)),
        Err(CallbackError)
    );
    let mut duplicate = progress();
    record_resize(&mut duplicate, 2, (640, 480)).unwrap();
    assert_eq!(
        record_resize(&mut duplicate, 2, (640, 480)),
        Err(CallbackError)
    );
    assert_eq!(record(&mut duplicate, 2), Err(CallbackError));
    assert_eq!(duplicate.window_resizes.len(), 1);
}
