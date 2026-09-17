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
        clipboard: Ok(frd::native_clipboard::Cleanup::NotStarted),
    }
}
#[test]
fn each_native_owner_must_finish_and_original_errors_are_preserved() {
    let mut report = report();
    assert_eq!(cleaned(&report), Ok(true));
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
