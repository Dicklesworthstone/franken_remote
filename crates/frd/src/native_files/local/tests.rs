//! Actual path selection, original intake custody and retained command controls.
//! The real ATP publication path has separate controlled-session fixtures.
use super::*;
use asupersync::{runtime::RuntimeBuilder, types::Budget};
use ipc::Handler as _;
use std::time::Duration;

fn paths() -> Vec<PathBuf> {
    vec![PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/Cargo.toml"
    ))]
}
fn handler(intake: HostSendIntake) -> Selections {
    Selections {
        intake,
        records: Mutex::new(Vec::with_capacity(MAX_HISTORY)),
    }
}
fn until() -> Instant {
    Instant::now().checked_add(Duration::from_secs(2)).unwrap()
}

#[test]
fn original_attachment_readiness_precedes_local_path_selection() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let bridge = HostSendIntake::new();
    let handler = handler(bridge.clone());
    let stopped = AtomicBool::new(false);
    // Before binding/readiness even a missing path cannot be touched as a send.
    let missing = vec![PathBuf::from("/proc/self/fr-file-command-does-not-exist")];
    assert_eq!(
        handler.select(&missing, until(), &stopped),
        Err(Error::Busy)
    );
    let intake = bridge
        .bind(runtime.request_cx_with_budget(Budget::INFINITE))
        .unwrap();
    assert_eq!(
        handler.select(&missing, until(), &stopped),
        Err(Error::Busy)
    );
    intake.set_ready(true);
    assert_eq!(
        handler.select(&missing, until(), &stopped),
        Err(Error::Selection {
            index: 0,
            reason: SelectionFailure::Missing
        })
    );
    assert_eq!(
        handler.select(&[PathBuf::from("/proc/self/exe")], until(), &stopped),
        Err(Error::Selection {
            index: 0,
            reason: SelectionFailure::Symlink
        })
    );
    assert!(intake.take().is_none());
    assert_eq!(handler.status(None).unwrap().sequence, 0);
    bridge.close();
}

#[test]
fn actual_descriptor_selection_reaches_the_original_intake_without_publication_claims() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let bridge = HostSendIntake::new();
    let intake = bridge
        .bind(runtime.request_cx_with_budget(Budget::INFINITE))
        .unwrap();
    intake.set_ready(true);
    let handler = handler(bridge.clone());
    let accepted = handler
        .select(&paths(), until(), &AtomicBool::new(false))
        .unwrap();
    let pending = intake.take().unwrap();
    assert_eq!(accepted.sequence, 1);
    assert_eq!(accepted.selected, 1);
    assert_eq!(pending.request.selection.len(), 1);
    assert_eq!(accepted.phase, Phase::WaitingForControl);
    assert_eq!(accepted.published, 0);
    assert_eq!(accepted.cleanup, Cleanup::Unknown);
    assert!(!accepted.complete);
    assert_eq!(handler.status(Some(1)).unwrap().sequence, 1);
    assert!(handler.cancel(1).unwrap().1);
    assert!(pending.control.snapshot().cancel_requested);
    assert_eq!(pending.control.snapshot().cleanup, None);
    bridge.close();
}

#[test]
fn history_exhaustion_preserves_all_original_controls_and_cannot_cancel_a_successor() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let bridge = HostSendIntake::new();
    let intake = bridge
        .bind(runtime.request_cx_with_budget(Budget::INFINITE))
        .unwrap();
    let handler = handler(bridge.clone());
    let stopped = AtomicBool::new(false);
    let mut latest = None;
    for index in 1..=MAX_HISTORY {
        intake.set_ready(true);
        let accepted = handler.select(&paths(), until(), &stopped).unwrap();
        assert_eq!(accepted.sequence, u64::try_from(index).unwrap());
        let pending = intake.take().unwrap();
        if index != MAX_HISTORY {
            // This storage test does not run a sender; mark prior controls
            // terminal without fabricating publication or cleanup receipts.
            pending
                .control
                .update(|status| status.phase = SendPhase::Finished);
        }
        latest = Some(pending.control);
    }
    intake.set_ready(true);
    assert_eq!(
        handler.select(&paths(), until(), &stopped),
        Err(Error::Exhausted)
    );
    assert!(intake.take().is_none());
    assert!(!handler.status(None).unwrap().ready);
    assert_eq!(handler.status(Some(1)).unwrap().phase, Phase::Finished);
    assert!(!handler.cancel(1).unwrap().1);
    assert!(!latest.as_ref().unwrap().snapshot().cancel_requested);
    assert!(
        handler
            .cancel(u64::try_from(MAX_HISTORY).unwrap())
            .unwrap()
            .1
    );
    assert!(latest.unwrap().snapshot().cancel_requested);
    assert_eq!(handler.status(Some(1)).unwrap().published, 0);
    assert_eq!(handler.status(Some(1)).unwrap().cleanup, Cleanup::Unknown);
    bridge.close();
}

#[test]
fn expired_command_and_terminal_original_owner_never_enqueue_selected_descriptors() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let bridge = HostSendIntake::new();
    let intake = bridge
        .bind(runtime.request_cx_with_budget(Budget::INFINITE))
        .unwrap();
    intake.set_ready(true);
    let handler = handler(bridge.clone());
    let stopped = AtomicBool::new(false);
    assert_eq!(
        handler.select(&paths(), Instant::now(), &stopped),
        Err(Error::Timeout)
    );
    assert!(intake.take().is_none());
    bridge.close();
    assert_eq!(
        handler.select(&paths(), until(), &stopped),
        Err(Error::Closed)
    );
    assert!(intake.take().is_none());
    let status = handler.status(None).unwrap();
    assert_eq!(status.sequence, 0);
    assert!(status.closed && !status.ready);
}
