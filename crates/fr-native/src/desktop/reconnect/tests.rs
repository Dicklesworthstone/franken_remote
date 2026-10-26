//! Cleanup/epoch policy fixtures; these are not proof of a live decoder's exit.
use super::*;
use std::path::Path;

fn session(epoch: u128) -> Session<()> {
    Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, epoch).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        (),
    )
}
#[test]
fn every_attempt_requires_new_epoch_and_completed_old_ownership() {
    let mut session = session(71);
    assert!(matches!(
        session.configuration(0),
        Err(ObserverError::Order)
    ));
    assert!(matches!(
        session.configuration(33),
        Err(ObserverError::Order)
    ));
    assert_eq!(session.configuration(1).unwrap().worker_epoch, 71);
    assert_eq!(session.configuration(32).unwrap().worker_epoch, 102);
    // Discovery can fail before the application is called; skipped attempt
    // numbers are allowed but neither reuse nor reversal is.
    session.last_attempt = 4;
    assert!(matches!(
        session.configuration(4),
        Err(ObserverError::Order)
    ));
    assert!(matches!(
        session.configuration(3),
        Err(ObserverError::Order)
    ));
    let configuration = session.configuration(5).unwrap();
    assert_eq!(configuration.worker_epoch, 75);
    session.desktop = Some(Desktop::new(configuration));
    assert!(matches!(
        session.configuration(5),
        Err(ObserverError::Order)
    ));
    session.desktop.as_mut().unwrap().close();
    assert!(matches!(
        session.configuration(6),
        Err(ObserverError::Order)
    ));
    // A stopped owner is NOT a collected owner. Only completed cleanup removes it.
    assert_eq!(
        session.desktop.as_ref().unwrap().state(),
        super::super::State::Stopped
    );
}
#[test]
fn epoch_overflow_never_reuses_the_previous_worker_generation() {
    let session = session(u128::MAX);
    assert_eq!(session.configuration(1).unwrap().worker_epoch, u128::MAX);
    assert!(matches!(
        session.configuration(2),
        Err(ObserverError::Order)
    ));
}
fn report() -> Cleanup {
    Cleanup {
        media: Ok(None),
        input: CaptureCleanup::NotStarted,
        window: WindowCleanup::NotStarted,
        picker: PickerCleanup::NotStarted,
        clipboard: Ok(frd::native_clipboard::Cleanup::NotStarted),
        files: Ok(()),
        file_result: None,
    }
}
#[test]
fn each_native_owner_must_finish_and_original_errors_are_preserved() {
    let mut report = report();
    assert_eq!(cleaned(&report), Ok(true));
    report.picker = PickerCleanup::Pending;
    assert_eq!(cleaned(&report), Ok(false));
    report.picker = PickerCleanup::Complete;
    report.input = CaptureCleanup::Pending;
    assert_eq!(cleaned(&report), Ok(false));
    report.input = CaptureCleanup::Complete;
    report.window = WindowCleanup::Pending;
    assert_eq!(cleaned(&report), Ok(false));
    report.window = WindowCleanup::Complete(crate::viewer_window::StopReason::SessionEnded);
    report.clipboard = Ok(frd::native_clipboard::Cleanup::Pending);
    assert_eq!(cleaned(&report), Ok(false));
    // Finished(Err) means the foreign thread actually ended, not that the
    // clipboard operation succeeded. Preserve the error inside the receipt.
    report.clipboard = Ok(frd::native_clipboard::Cleanup::Finished(Err(
        frd::clipboard_quic::Error::Cancelled,
    )));
    assert_eq!(cleaned(&report), Ok(true));
    assert!(matches!(
        report.clipboard,
        Ok(frd::native_clipboard::Cleanup::Finished(Err(_)))
    ));
    report.clipboard = Err(frd::clipboard_quic::Error::Cancelled);
    assert_eq!(cleaned(&report), Err(CleanupFailure::Clipboard));
    report.media = Err(frd::media::Error::Backpressure);
    assert_eq!(cleaned(&report), Err(CleanupFailure::Media));
}

#[test]
fn native_picker_policy_survives_but_choices_and_owners_do_not_cross_attempts() {
    let mut original = session(71);
    assert!(!original.configuration(1).unwrap().display_picker);
    original.configuration = original.configuration.with_display_picker();
    for attempt in [1, 2, 32] {
        let next = original.configuration(attempt).unwrap();
        assert!(next.display_picker);
        let desktop = Desktop::new(next);
        assert!(desktop.picker().is_none());
        assert!(!desktop.renderer_started);
    }
}

#[test]
fn shortcut_capture_request_survives_fresh_attempts_without_creating_input() {
    let mut original = session(71);
    assert!(!original.configuration(1).unwrap().capture_system_shortcuts);
    original.configuration = original.configuration.with_system_shortcut_capture();
    for attempt in [1, 2, 32] {
        let next = original.configuration(attempt).unwrap();
        assert!(next.capture_system_shortcuts);
        assert_eq!(next.worker_epoch, 70 + u128::from(attempt));
        // Retaining local intent is neither native acquisition nor authority.
        let desktop = Desktop::new(next);
        assert!(desktop.input().is_none());
        assert!(desktop.window().is_none());
        assert!(desktop.stop.is_none());
    }
}

#[test]
fn a_file_cleanup_failure_blocks_next_attempt_and_keeps_its_original_receipt() {
    use frd::session_startup::{FileSendError, FileSendReceipt};
    let mut report = report();
    // This fixture isolates the policy, not actual source I/O. A real receipt
    // from the original sender is retained by Desktop's cleanup implementation.
    report.file_result = Some(FileSendReceipt {
        id: 17,
        outcome: frd::session_startup::FileSendOutcome::PublicationUnknown,
    });
    for reason in [
        FileSendError::Expired,
        FileSendError::Cancelled,
        FileSendError::Worker,
    ] {
        report.files = Err(reason);
        assert_eq!(cleaned(&report), Err(CleanupFailure::Files));
        assert_eq!(report.files, Err(reason));
        assert_eq!(report.file_result.unwrap().id, 17);
    }
    report.files = Ok(());
    assert_eq!(cleaned(&report), Ok(true));
    assert_eq!(
        report.file_result.unwrap().outcome,
        frd::session_startup::FileSendOutcome::PublicationUnknown
    );
}

#[test]
fn fit_policy_survives_reconnect_without_reusing_native_owners_or_input() {
    let mut original = session(71);
    assert_eq!(original.configuration(1).unwrap().fit_window, None);
    assert!(!original.configuration(1).unwrap().clipboard_space);
    original.configuration = original
        .configuration
        .with_fitted_window(960, 540)
        .unwrap()
        .with_clipboard_space();
    for attempt in [1, 2, 32] {
        let next = original.configuration(attempt).unwrap();
        assert_eq!(next.fit_window, Some((960, 540)));
        assert!(next.clipboard_space);
        assert_eq!(next.worker_epoch, 70 + u128::from(attempt));
        let desktop = Desktop::new(next);
        assert!(desktop.window().is_none());
        assert!(!desktop.renderer_started);
        assert!(desktop.stop.is_none());
    }
    for (width, height) in [(0, 540), (959, 540), (960, 539), (u32::MAX, 540)] {
        assert!(
            session(71)
                .configuration
                .with_fitted_window(width, height)
                .is_err()
        );
    }
}

#[derive(Default)]
struct SummaryUi(Vec<(u8, CleanupSummary)>);
impl Ui for SummaryUi {
    fn choose(&mut self, _: u8, _: &Catalog) -> Result<Option<u128>, CallbackError> {
        Ok(None)
    }
    fn approval(&mut self, _: u8, _: ApprovalNotice) -> Result<(), CallbackError> {
        Ok(())
    }
    fn cleanup_finished(&mut self, attempt: u8, summary: CleanupSummary) {
        self.0.push((attempt, summary));
    }
}

#[test]
fn cleanup_summary_is_informational_and_keeps_the_original_stop_on_failure() {
    // No native owner is started. This checks the real callback ordering and
    // exact metadata/result propagation, not an OS cleanup receipt.
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap();
    for cancelled in [false, true] {
        let mut session = Session::new(
            Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
            ObserverPolicy::default(),
            Mode::Observe,
            SummaryUi::default(),
        );
        let configuration = session.configuration(4).unwrap();
        session.last_attempt = 4;
        session.input_stop = Some(crate::viewer_input::StopReason::LocalEscape);
        let cx = runtime.request_cx_with_budget(Budget::INFINITE);
        let deadline = Deadline::after(&cx, std::time::Duration::from_secs(1)).unwrap();
        if cancelled {
            session.desktop = Some(Desktop::new(configuration));
            cx.cancel_fast(asupersync::types::CancelKind::User);
        }
        let result = runtime.block_on(session.cleanup(&cx, deadline));
        assert_eq!(result.is_err(), cancelled);
        assert_eq!(session.ui.0.len(), 1);
        let (attempt, summary) = session.ui.0[0];
        assert_eq!(attempt, 4);
        assert_eq!(
            summary.input_stop,
            Some(crate::viewer_input::StopReason::LocalEscape)
        );
        assert_eq!(
            summary.failure,
            if cancelled {
                Some(CleanupFailure::Cancelled)
            } else {
                None
            }
        );
        assert_eq!(summary.failure, session.cleanup_failure());
    }
}

#[derive(Default)]
struct OfferUi {
    seen: Vec<u8>,
    refuse: bool,
}
impl Ui for OfferUi {
    fn offer(&mut self, attempt: u8, original: &Offer) -> Result<Offer, CallbackError> {
        self.seen.push(attempt);
        if self.refuse {
            return Err(CallbackError);
        }
        let mut narrowed = original.clone();
        narrowed
            .capabilities
            .retain(|capability| capability.required);
        Ok(narrowed)
    }
    fn choose(&mut self, _: u8, _: &Catalog) -> Result<Option<u128>, CallbackError> {
        Ok(None)
    }
    fn approval(&mut self, _: u8, _: ApprovalNotice) -> Result<(), CallbackError> {
        Ok(())
    }
}

#[test]
fn default_ui_offer_is_unchanged_and_native_forwarding_creates_no_owner() {
    let original = fr_client::native::observation_offer_with_audio();
    assert_eq!(
        Ui::offer(&mut SummaryUi::default(), 1, &original),
        Ok(original.clone())
    );
    let mut session = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        OfferUi::default(),
    );
    let narrowed = Application::offer(&mut session, 2, &original).unwrap();
    let mut expected = original.clone();
    expected
        .capabilities
        .retain(|capability| capability.required);
    assert_eq!(narrowed, expected);
    assert!(narrowed.capabilities.len() < original.capabilities.len());
    assert_eq!(session.ui.seen, [2]);
    assert_eq!(session.last_attempt, 0);
    assert!(session.desktop.is_none());
    assert!(!session.opened);
    assert!(session.cleanup.is_none());

    session.ui.refuse = true;
    assert_eq!(
        Application::offer(&mut session, 3, &original),
        Err(CallbackError)
    );
    assert_eq!(session.ui.seen, [2, 3]);
    assert!(session.desktop.is_none());
}

#[test]
fn offer_forwarding_cannot_bypass_old_native_ownership_or_attempt_bounds() {
    let original = fr_client::native::observation_offer();
    let mut session = Session::new(
        Configuration::new(Path::new("/usr/bin/false"), ":0", None, 71).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        OfferUi::default(),
    );
    session.last_attempt = 4;
    for attempt in [0, 1, 4, 33] {
        assert_eq!(
            Application::offer(&mut session, attempt, &original),
            Err(CallbackError)
        );
    }
    session.desktop = Some(Desktop::new(session.configuration(5).unwrap()));
    for attempt in [5, 6] {
        assert_eq!(
            Application::offer(&mut session, attempt, &original),
            Err(CallbackError)
        );
    }
    assert!(session.ui.seen.is_empty());
    assert!(session.desktop.is_some());
}
