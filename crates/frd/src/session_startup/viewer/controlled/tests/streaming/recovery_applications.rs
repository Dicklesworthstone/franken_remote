//! Actual TLS/UDP, original file application owners, ATP and disk workers.
//! Video decode and the input sink are explicit fixtures; no replacement video
//! offer is sent, so local file actions must complete inside the recovery wait.
use super::*;
use crate::{
    native_files::{Directory, Limits, Selection, SendPhase},
    session_startup::{StreamingViewerError, running::streaming::files::Lane as HostFiles},
};
use fr_files::sender::{Error as SendError, Outcome, Stage, batch::Stop};
use std::fs;

use super::super::files::Disk;

fn lose_reference(f: &mut Fixture, cx: &Cx) -> u64 {
    let at = now(cx).unwrap();
    let mut bytes = [0; 1150];
    let n = encode_progress(
        Progress {
            descriptor: FrameDescriptor {
                frame: 1,
                reference: Some(0),
                total_bytes: 4,
                stride: 4,
                capture_micros: at,
            },
            observed_micros: at,
            observation: SourceObservation::Captured,
            pipeline: PipelineState::Running,
        },
        f.host_media.bindings().for_channel(Channel::MediaConfig),
        &f.host_media.limits(),
        &mut bytes,
    )
    .unwrap();
    f.receiver
        .receive(Channel::MediaConfig, &bytes[..n], at)
        .unwrap();
    at
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    WaitingForRecovery,
    Resuming,
    Pausing,
    Cancelling,
    Cleaned,
}

#[test]
#[allow(clippy::too_many_lines)]
fn recovery_keeps_original_file_resume_pause_cancel_and_cleanup_live() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut f = Box::pin(fixture_with_recovery(
            &c,
            &h,
            caps(),
            true,
            false,
            true,
            ClipboardMode::FileDrop,
            true,
        ))
        .await;
        let source = Disk::new();
        let dest = Disk::new();
        let mut random = 0x5eed_u64;
        let first: Vec<_> = (0..1_000_000)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                random.to_le_bytes()[3]
            })
            .collect();
        fs::write(source.0.join("first.bin"), first).unwrap();
        fs::write(source.0.join("second.bin"), b"must never start").unwrap();
        let selection =
            Selection::select(&[source.0.join("first.bin"), source.0.join("second.bin")]).unwrap();
        let directory = Directory::open(&dest.0, Limits::default()).unwrap();
        let mut host_files = HostFiles::default();
        host_files.add_upload(&directory);
        let view = f.host_media.binding();
        let binding = f.viewer.input_result_binding();
        let controller = f.viewer.control();
        let lost_at = lose_reference(&mut f, &c);
        let policy = ReceivePolicy::default();
        let until = lost_at + policy.reference_budget_micros + policy.recovery_budget_micros;
        let mut viewer = f
            .viewer
            .into_streaming(f.presenter.take().unwrap(), f.receiver)
            .unwrap();
        let (send, intake) = viewer
            .configure_file_send_with_intake(selection.request().unwrap())
            .unwrap();
        // The original application must settle its attachment and queue the
        // selection once, but no source may start before the explicit Resume.
        assert!(send.pause());
        let worker = viewer.worker_id();
        let done = crate::test_cell::SendCell::new(false);
        let reports = crate::test_cell::SendCell::new(0);
        let mut step = Step::WaitingForRecovery;
        let mut suspension = None;
        let mut original_source = None;
        let mut cancelled_report = None;
        let repair = Route::Stream(f.host_media.repair_stream(f.host.io().unwrap().0).unwrap());
        let driver = f.driver.take().unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let (result, ()) = Box::pin(support::both(
                    async {
                        let result = viewer
                            .serve(
                                |owner, event| {
                                    assert!(
                                        now(&c).unwrap() < until,
                                        "file owner stalled in {step:?}: {:?}",
                                        send.snapshot()
                                    );
                                    assert!(event.is_none());
                                    let owner = owner.expect("original controller disappeared");
                                    assert_eq!(owner.input_result_binding(), binding);
                                    assert!(!controller.is_stopped());
                                    if owner.media.is_some() || reports.get() == 0 {
                                        return Ok(());
                                    }
                                    assert_eq!(reports.get(), 1);
                                    let since = owner
                                        .input
                                        .suspended_since()
                                        .expect("recovery must keep input suspended");
                                    assert_eq!(*suspension.get_or_insert(since), since);
                                    assert!(owner.action(key(true)).is_err());
                                    assert_eq!(owner.pending_actions(), 0);
                                    match step {
                                        Step::WaitingForRecovery => {
                                            if send.status().phase == SendPhase::Sending {
                                                assert_eq!(owner.file_stage(), Some(Stage::Queued));
                                                assert_eq!(owner.file_is_paused(), Some(true));
                                                assert_eq!(
                                                    owner.file_batch_report().unwrap().started,
                                                    0
                                                );
                                                assert!(send.resume());
                                                step = Step::Resuming;
                                            }
                                        }
                                        Step::Resuming => {
                                            if let Some(progress) = owner.file_progress()
                                                && progress.queued_bytes > 0
                                            {
                                                assert_eq!(owner.file_is_paused(), Some(false));
                                                assert_eq!(progress.total_bytes, Some(1_000_000));
                                                assert!(progress.queued_bytes < 1_000_000);
                                                original_source = Some(progress.id);
                                                assert!(send.pause());
                                                step = Step::Pausing;
                                            }
                                        }
                                        Step::Pausing => {
                                            if send.snapshot().paused == Some(true) {
                                                assert_eq!(owner.file_is_paused(), Some(true));
                                                assert_eq!(
                                                    owner.file_progress().unwrap().id,
                                                    original_source.unwrap()
                                                );
                                                assert!(send.cancel());
                                                step = Step::Cancelling;
                                            }
                                        }
                                        Step::Cancelling => {
                                            let snapshot = send.snapshot();
                                            if snapshot.cleanup == Some(Ok(()))
                                                && let Some(report) = snapshot.status.report
                                                && report.complete
                                            {
                                                assert_eq!(report.total, 2);
                                                assert_eq!(report.started, 1);
                                                assert_eq!(report.not_started(), 1);
                                                assert_eq!(
                                                    report.stop,
                                                    Some(Stop::Local(SendError::Cancelled))
                                                );
                                                let receipts: Vec<_> = report.receipts().collect();
                                                assert_eq!(receipts.len(), 1);
                                                assert_eq!(
                                                    receipts[0].id,
                                                    original_source.unwrap()
                                                );
                                                assert_eq!(
                                                    receipts[0].outcome,
                                                    Outcome::InterruptedBeforePublication(
                                                        SendError::Cancelled
                                                    )
                                                );
                                                assert!(owner.file_cleanup_finished());
                                                assert!(intake.is_closed());
                                                assert!(!send.resume());
                                                cancelled_report = Some(report);
                                                step = Step::Cleaned;
                                                return Err(());
                                            }
                                        }
                                        Step::Cleaned => panic!("served after application exit"),
                                    }
                                    Ok(())
                                },
                                |_| panic!("recovery must not invent an input result"),
                                block,
                            )
                            .await;
                        done.set(true);
                        result
                    },
                    async {
                        let (mut nonces, mut tickets) = (160_000, 170_000);
                        while !done.get() {
                            host_files.host(&mut f.host, view, &mut || nonce(&mut nonces));
                            let result = f
                                .host
                                .drive(
                                    Duration::from_millis(1),
                                    || nonce(&mut nonces),
                                    || {
                                        tickets += 1;
                                        Some(InputTicketId::from_raw(tickets))
                                    },
                                    |route, bytes| {
                                        if bytes.get(6..8)
                                            == Some(&(Kind::RecoveryRequest as u16).to_be_bytes())
                                        {
                                            reports.set(reports.get() + 1);
                                            // Deliberately withhold the next configuration.
                                            return Ok(Disposition::Consumed);
                                        }
                                        if route == repair {
                                            return Ok(Disposition::Consumed);
                                        }
                                        block(route, bytes)
                                    },
                                )
                                .await;
                            if done.get() {
                                break;
                            }
                            result.unwrap();
                        }
                        f.host.close();
                    },
                ))
                .await;
                assert_eq!(result, Err(StreamingViewerError::Application));
                assert_eq!(step, Step::Cleaned);
                assert_eq!(reports.get(), 1);
                assert!(controller.is_stopped());
                assert_eq!(viewer.worker_id(), worker);
                assert_eq!(viewer.file_batch_report(), cancelled_report);
                viewer
                    .reap_files(
                        &cleanup,
                        crate::worker::Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
                    )
                    .await
                    .unwrap();
                viewer
                    .reap_media(
                        &cleanup,
                        crate::worker::Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
                    )
                    .await
                    .unwrap();
                let host_cleanup = f
                    .host
                    .reap_files(
                        &cleanup,
                        crate::worker::Deadline::after(&cleanup, Duration::from_secs(1)).unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    host_cleanup,
                    crate::session_startup::running::controlled::files::Cleanup::Finished(Ok(()))
                );
                assert_eq!(viewer.file_batch_report(), cancelled_report);
                assert_eq!(send.status().report, cancelled_report);
                assert_eq!(send.snapshot().cleanup, Some(Ok(())));
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert!(f.effects.lock().unwrap().keys.is_empty());
                f.observation.revoke();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!f.seat.is_occupied());
    });
}
