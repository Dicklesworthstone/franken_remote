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
    Selections::new(Intake::Host(intake))
}
fn until() -> Instant {
    Instant::now().checked_add(Duration::from_secs(2)).unwrap()
}

#[test]
fn displayed_command_control_keeps_its_original_batch_when_a_successor_arrives() {
    let first = SendControl::new();
    let results = Results(Arc::new(Mutex::new(vec![Record {
        control: first.clone(),
        selected: 1,
        sizes: vec![Some(10)],
    }])));
    let (sequence, displayed) = results.latest_control().unwrap();
    assert_eq!(sequence, 1);
    let second = SendControl::new();
    results.records().push(Record {
        control: second.clone(),
        selected: 1,
        sizes: vec![Some(20)],
    });
    assert_eq!(results.latest_control().unwrap().0, 2);

    // A UI click on the previously displayed row must not look up the newly
    // selected command. The retained handle is the real production control.
    assert!(displayed.pause());
    assert!(first.snapshot().pause_requested);
    assert!(!second.snapshot().pause_requested);
    assert_eq!(first.snapshot().paused, None);
    first.observe(None, Some(sender::Stage::Queued), None, Some(true));
    assert!(displayed.resume());
    assert!(!first.snapshot().pause_requested);
    assert_eq!(first.snapshot().paused, Some(true));
    assert_eq!(second.snapshot().paused, None);
    assert!(
        results
            .batches()
            .iter()
            .all(|batch| batch.status.report.is_none())
    );
}

#[test]
fn displayed_command_pacing_cannot_reactivate_cancelled_or_terminal_owners() {
    for phase in [
        SendPhase::WaitingForControl,
        SendPhase::Finished,
        SendPhase::Ended,
    ] {
        let original = SendControl::new();
        assert!(original.pause());
        let results = Results(Arc::new(Mutex::new(vec![Record {
            control: original.clone(),
            selected: 1,
            sizes: vec![None],
        }])));
        let (_, displayed) = results.latest_control().unwrap();
        if phase == SendPhase::WaitingForControl {
            assert!(original.cancel());
        } else {
            original.update(|status| status.phase = phase);
        }
        assert!(!displayed.resume());
        assert!(!displayed.pause());
        assert_eq!(original.snapshot().paused, None);
        assert_eq!(original.snapshot().cleanup, None);
        assert!(original.status().report.is_none());
    }
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

#[test]
fn upload_commands_use_original_readiness_and_cancel_their_taken_batch_on_close() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        runtime.request_cx_with_budget(Budget::INFINITE),
        sender::Policy::default(),
    );
    let handler = Selections::new(Intake::Upload(intake.clone()));
    let stopped = AtomicBool::new(false);
    assert_eq!(
        handler.select(&["/missing-not-selected".into()], until(), &stopped),
        Err(Error::Busy)
    );
    intake.set_ready(true);
    let admitted = handler.select(&paths(), until(), &stopped).unwrap();
    let pending = intake.take().unwrap();
    assert_eq!(admitted.sequence, 1);
    assert_eq!(pending.request.selection.len(), 1);
    assert_eq!(admitted.published, 0);
    let results = handler.results.clone();
    handler.close();
    assert!(intake.is_closed());
    assert!(pending.control.snapshot().cancel_requested);
    assert_eq!(pending.control.snapshot().cleanup, None);
    intake.set_ready(true);
    assert_eq!(
        handler.select(&paths(), until(), &stopped),
        Err(Error::Closed)
    );
    assert!(intake.take().is_none());
    drop(handler);
    let batches = results.batches();
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].sequence, admitted.sequence);
    assert_eq!(batches[0].sizes.len(), 1);
    assert!(batches[0].sizes[0].is_some());
    assert!(
        batches[0].status.report.is_none(),
        "cleanup and cancellation do not create publication evidence"
    );
}

#[test]
fn old_upload_request_cancellation_cannot_target_a_new_command_or_drop() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        runtime.request_cx_with_budget(Budget::INFINITE),
        sender::Policy::default(),
    );
    let handler = Selections::new(Intake::Upload(intake.clone()));
    intake.set_ready(true);
    handler
        .select(&paths(), until(), &AtomicBool::new(false))
        .unwrap();
    let first = intake.take().unwrap();
    first
        .control
        .update(|status| status.phase = SendPhase::Finished);
    intake.set_ready(true);
    // Another explicit surface shares the slot, not this command's history.
    let dropped = intake
        .submit(Selection::select(&paths()).unwrap().request().unwrap())
        .unwrap();
    let _drop = intake.take().unwrap();
    assert!(!handler.cancel(1).unwrap().1);
    assert_eq!(handler.cancel(2), Err(Error::UnknownRequest));
    assert!(!dropped.snapshot().cancel_requested);
    handler.close();
    assert!(
        !dropped.snapshot().cancel_requested,
        "stopping this command worker does not cancel an unrelated taken batch"
    );
    assert_eq!(handler.status(Some(1)).unwrap().phase, Phase::Finished);
    assert_eq!(handler.results.batches()[0].sequence, 1);
}

#[test]
fn upload_pacing_preserves_original_pending_selection_expiry_and_observed_gate() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        runtime.request_cx_with_budget(Budget::INFINITE),
        sender::Policy::default(),
    );
    let handler = Selections::new(Intake::Upload(intake.clone()));
    intake.set_ready(true);
    handler
        .select(&paths(), until(), &AtomicBool::new(false))
        .unwrap();
    let pending = intake.take().unwrap();
    let original_until = pending.until_us;
    let original_lifetime = pending.request.lifetime;
    let paused = handler.set_paused(1, true).unwrap();
    assert!(paused.1);
    assert_eq!(paused.0.pause_requested, Some(true));
    assert_eq!(paused.0.paused, None);
    assert!(!intake.ready());
    assert_eq!(
        handler.select(&paths(), until(), &AtomicBool::new(false)),
        Err(Error::Busy),
        "pausing does not admit a successor"
    );
    pending
        .control
        .observe(None, Some(sender::Stage::Streaming), None, Some(false));
    let observed = handler.status(Some(1)).unwrap();
    assert_eq!(observed.pause_requested, Some(true));
    assert_eq!(observed.paused, Some(false));
    pending
        .control
        .observe(None, Some(sender::Stage::Streaming), None, Some(true));
    let resumed = handler.set_paused(1, false).unwrap();
    assert!(resumed.1);
    assert_eq!(resumed.0.pause_requested, Some(false));
    assert_eq!(
        resumed.0.paused,
        Some(true),
        "resume cannot fabricate a native acknowledgement"
    );
    assert_eq!(pending.until_us, original_until);
    assert_eq!(pending.request.lifetime, original_lifetime);
    assert!(intake.take().is_none(), "no command resubmits a selection");
    assert_eq!(handler.results.batches().len(), 1);
    assert_eq!(resumed.0.published, 0);
    assert_eq!(resumed.0.cleanup, Cleanup::Unknown);
    handler.close();
}

#[test]
fn upload_pacing_cannot_revive_cancelled_or_terminal_request_or_touch_unrelated_drop() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let intake = SendIntake::new(
        runtime.request_cx_with_budget(Budget::INFINITE),
        sender::Policy::default(),
    );
    let handler = Selections::new(Intake::Upload(intake.clone()));
    intake.set_ready(true);
    handler
        .select(&paths(), until(), &AtomicBool::new(false))
        .unwrap();
    let first = intake.take().unwrap();
    assert!(handler.set_paused(1, true).unwrap().1);
    handler.cancel(1).unwrap();
    assert!(!handler.set_paused(1, false).unwrap().1);
    assert!(first.control.snapshot().pause_requested);
    first
        .control
        .update(|status| status.phase = SendPhase::Finished);
    intake.set_ready(true);
    let dropped = intake
        .submit(Selection::select(&paths()).unwrap().request().unwrap())
        .unwrap();
    let _drop = intake.take().unwrap();
    assert!(!handler.set_paused(1, false).unwrap().1);
    assert_eq!(handler.set_paused(0, true), Err(Error::InvalidRequest));
    assert_eq!(handler.set_paused(2, true), Err(Error::UnknownRequest));
    assert!(!dropped.snapshot().pause_requested);
    assert!(!dropped.snapshot().cancel_requested);
    handler.close();
    assert_eq!(handler.set_paused(1, false), Err(Error::Closed));
    assert!(!dropped.snapshot().pause_requested);
}

#[test]
fn host_socket_pacing_preserves_original_batch_expiry_and_fences_successors() {
    let runtime = RuntimeBuilder::current_thread().build().unwrap();
    let bridge = HostSendIntake::new();
    let intake = bridge
        .bind(runtime.request_cx_with_budget(Budget::INFINITE))
        .unwrap();
    intake.set_ready(true);
    let owner = Owner::bind(
        &format!("host-pacing-{}", std::process::id()),
        bridge.clone(),
    )
    .unwrap();
    let info = owner.info().unwrap();
    let first = send(&info.target, &info.instance, &paths()).unwrap();
    let pending = intake.take().unwrap();
    let original_until = pending.until_us;
    let original_lifetime = pending.request.lifetime;

    let paused = pause(&info.target, &info.instance, first.sequence).unwrap();
    assert!(paused.changed);
    assert_eq!(paused.info.status.pause_requested, Some(true));
    assert_eq!(paused.info.status.paused, None);
    assert!(pending.control.snapshot().pause_requested);
    assert!(!paused.info.status.ready);
    assert_eq!(
        send(&info.target, &info.instance, &paths()),
        Err(Error::Busy),
        "a paused original selection keeps the intake occupied"
    );
    assert!(
        !pause(&info.target, &info.instance, first.sequence)
            .unwrap()
            .changed
    );
    // A sender observation can lag the command, and stays independent when
    // the same original control is resumed. No publication is fabricated.
    pending
        .control
        .observe(None, Some(sender::Stage::Streaming), None, Some(true));
    let resumed = resume(&info.target, &info.instance, first.sequence).unwrap();
    assert!(resumed.changed);
    assert_eq!(resumed.info.status.pause_requested, Some(false));
    assert_eq!(resumed.info.status.paused, Some(true));
    assert_eq!(resumed.info.status.published, 0);
    assert_eq!(resumed.info.status.cleanup, Cleanup::Unknown);
    assert_eq!(pending.until_us, original_until);
    assert_eq!(pending.request.lifetime, original_lifetime);
    assert!(intake.take().is_none());

    // This fixture does not run ATP: finish the first control without claiming
    // any cleanup or receipt, then admit a genuinely new explicit selection.
    pending
        .control
        .update(|status| status.phase = SendPhase::Finished);
    intake.set_ready(true);
    let second = send(&info.target, &info.instance, &paths()).unwrap();
    let successor = intake.take().unwrap();
    assert_eq!(second.sequence, first.sequence + 1);
    assert!(
        !pause(&info.target, &info.instance, first.sequence)
            .unwrap()
            .changed
    );
    assert_eq!(
        pause(&info.target, &info.instance, second.sequence + 1),
        Err(Error::UnknownRequest)
    );
    assert!(!successor.control.snapshot().pause_requested);
    assert!(!successor.control.snapshot().cancel_requested);
    assert_eq!(owner.results().batches().len(), 2);
    pause(&info.target, &info.instance, second.sequence).unwrap();
    cancel(&info.target, &info.instance, second.sequence).unwrap();
    assert!(
        !resume(&info.target, &info.instance, second.sequence)
            .unwrap()
            .changed
    );
    assert!(successor.control.snapshot().pause_requested);
    assert!(successor.control.snapshot().cancel_requested);
    owner.stop();
    assert!(successor.control.snapshot().cancel_requested);
    let deadline = until();
    while !owner.is_complete() {
        assert!(
            Instant::now() < deadline,
            "original endpoint workers did not join"
        );
        std::thread::yield_now();
    }
}
