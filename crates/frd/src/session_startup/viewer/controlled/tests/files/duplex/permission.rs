//! Real duplex owners, ATP and TLS/UDP. Pause source admission only to let the
//! existing transport prove its own empty entry; never replace/drop pending I/O.
use super::*;
use std::{
    cell::Cell,
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
};

async fn quiet(state: &mut Fixture, c: &Cx, h: &Cx) {
    let until = now(c).unwrap() + 200_000;
    let host_control = state.host.control();
    loop {
        let host = state.host.io().unwrap().0;
        let viewer = &mut state.viewer;
        if host.usage().retained_send_records == 0
            && viewer.session.transport.usage().retained_send_records == 0
        {
            // This fixture has sent only reliable records, no datagrams. Their
            // credit is released only after EmptySendState::matches proves both
            // unsent and retransmission maps empty; successful drives also settle
            // protected output. Held still checks its own native entry witness.
            assert!(!host.is_closed() && !viewer.session.transport.is_closed());
            break;
        }
        assert!(now(c).unwrap() < until, "original payload must drain");
        // No application service runs in this short boundary fixture, so neither
        // direction can append a new source record while existing bytes drain.
        let (host, viewer) = Box::pin(support::both(
            host.drive(h, Duration::from_millis(1), || !host_control.is_stopped()),
            viewer
                .session
                .transport
                .drive(c, Duration::from_millis(1), || {
                    viewer.files.permits_io() && permitted(&mut viewer.input, &viewer.control, c)
                }),
        ))
        .await;
        host.unwrap();
        viewer.unwrap();
    }
}

async fn revoke_during_pending_receive(viewer: &mut ControlledViewer, c: &Cx, p: &Permission) {
    for _ in 0..16 {
        let checks = Cell::new(0);
        let mut io = pin!(
            viewer
                .session
                .transport
                .drive(c, Duration::from_millis(10), || {
                    checks.set(checks.get() + 1);
                    viewer.files.permits_io() && permitted(&mut viewer.input, &viewer.control, c)
                })
        );
        let pending = poll_fn(|task| match io.as_mut().poll(task) {
            Poll::Pending => Poll::Ready(true),
            Poll::Ready(result) => {
                result.unwrap();
                Poll::Ready(false)
            }
        })
        .await;
        if pending {
            // drive admission, prefix admission, flush, receive admission, then
            // the original receive poll. Finish an earlier pending flush intact.
            if checks.get() < 5 {
                io.await.unwrap();
                continue;
            }
            p.revoke();
            io.await.unwrap();
            return;
        }
    }
    panic!("must revoke during a genuinely pending native receive");
}

fn assert_download(state: &Fixture, destination: &Disk, expected: &[u8]) {
    let received = state.host.file_batch_report().unwrap();
    assert_eq!(received.stop, None);
    assert!(received.receipts().all(|r| matches!(
        r.outcome,
        Outcome::ControllerPublished {
            publication: Publication::Durable,
            ..
        }
    )));
    assert_eq!(fs::read(destination.0.join("download")).unwrap(), expected);
    assert_eq!(state.viewer.file_receive_setup_failure(), None);
    assert_eq!(state.effects.lock().unwrap().keys, [true]);
    assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
}

#[test]
fn revoked_upload_during_empty_native_wait_preserves_sibling_download_and_input() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_duplex(&c, &h)).await;
        let host_source = Disk::new();
        let viewer_source = Disk::new();
        let host_dest = Disk::new();
        let viewer_dest = Disk::new();
        let upload = vec![0x36; 1_000_007];
        let download = vec![0x87; 1_000_019];
        let permission = start(
            &mut state,
            &host_dest,
            &viewer_dest,
            &host_source,
            &download,
        );
        let lease = state.viewer.input.binding().lease;
        let connection = state.viewer.session.transport.binding();
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 5_000_000;
                attached(&mut state, &c, &h, &mut n, &mut t, until).await;
                state
                    .viewer
                    .send_file(viewer_source.source(&upload), "upload")
                    .unwrap();
                while !state
                    .viewer
                    .file_progress()
                    .is_some_and(|p| p.queued_bytes > 0 && p.queued_bytes < upload.len() as u64)
                {
                    assert!(now(&c).unwrap() < until, "bounded partial upload");
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                state.viewer.set_files_paused(true).unwrap();
                quiet(&mut state, &c, &h).await;
                assert!(!state.host.file_batch_report().unwrap().complete);
                revoke_during_pending_receive(&mut state.viewer, &c, &permission).await;
                assert!(state.viewer.session.transport.is_bound_to(&connection));
                assert!(!state.viewer.is_closed());
                assert_eq!(state.viewer.file_send_failure(), Some(SendError::Cancelled));
                let receipt = state.viewer.file_result().unwrap();
                assert_eq!(
                    receipt.outcome,
                    Outcome::InterruptedBeforePublication(SendError::Cancelled)
                );
                let _ = state.viewer.action(key(true)).unwrap();
                loop {
                    assert!(
                        now(&c).unwrap() < until,
                        "sibling download and source cleanup"
                    );
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    if state.host.file_batch_report().is_some_and(|r| r.complete)
                        && state.viewer.try_finish_file_send_cleanup() == Some(Ok(()))
                        && matches!(
                            state.host.file_receive_cleanup(),
                            crate::session_startup::FileReceiveCleanup::Finished(Ok(()))
                        )
                        && state.viewer.pending_actions() == 0
                    {
                        break;
                    }
                }
                assert_download(&state, &viewer_dest, &download);
                assert_eq!(fs::read_dir(&host_dest.0).unwrap().count(), 0);
                assert_eq!(state.viewer.file_result(), Some(receipt));
                assert_eq!(state.viewer.input.binding().lease, lease);
                assert_eq!(
                    state
                        .viewer
                        .send_file(File::open(viewer_source.0.join("source")).unwrap(), "retry"),
                    Err(SendError::Cancelled)
                );
                assert_eq!(state.viewer.take_file_result(), Some(receipt));
                download::retire(&mut state, &cleanup).await;
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn revoked_upload_after_native_staging_still_fences_the_parent() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        let permission = Permission::new(true);
        join(&mut state, &dest, permission.clone());
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                quiet(&mut state, &c, &h).await;
                state
                    .viewer
                    .send_file(source.source(b"selected upload"), "denied")
                    .unwrap();
                let until = now(&c).unwrap() + 500_000;
                while state.viewer.file_stage() != Some(Stage::AwaitingAcceptance) {
                    assert!(
                        now(&c).unwrap() < until,
                        "original source must prepare its offer"
                    );
                    let viewer = &mut state.viewer;
                    viewer
                        .files
                        .service(
                            &mut viewer.session.transport,
                            &mut viewer.input,
                            &viewer.control,
                            &c,
                        )
                        .unwrap();
                    if viewer.file_stage() != Some(Stage::AwaitingAcceptance) {
                        asupersync::time::sleep(c.now(), Duration::from_millis(1)).await;
                    }
                }
                let checks = Cell::new(0);
                let viewer = &mut state.viewer;
                let result = viewer
                    .session
                    .transport
                    .drive(&c, Duration::ZERO, || {
                        checks.set(checks.get() + 1);
                        if checks.get() == 3 {
                            permission.revoke();
                        }
                        viewer.files.permits_io()
                            && permitted(&mut viewer.input, &viewer.control, &c)
                    })
                    .await;
                assert_eq!(result, Err(quic::Error::OptionalRetirementDuringIo));
                assert_eq!(checks.get(), 3, "staged native payload cannot be resumed");
                assert!(viewer.session.transport.is_closed());
                assert_eq!(viewer.file_send_failure(), Some(SendError::Cancelled));
                assert_eq!(
                    viewer.file_result().unwrap().outcome,
                    Outcome::InterruptedBeforePublication(SendError::Cancelled)
                );
                assert!(!dest.0.join("denied").exists());
                download::retire(&mut state, &cleanup).await;
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}
