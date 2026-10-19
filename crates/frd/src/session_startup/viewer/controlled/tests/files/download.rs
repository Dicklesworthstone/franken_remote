//! Real ATP, disk publication, TLS/UDP and BOTH production control loops.
//! The desktop and native input sink remain the enclosing explicit fixtures.
use super::*;
use crate::native_files::{Absence, ReceiveControl};
use fr_files::{sender::batch::Selection, worker::Completion};
use fr_transport::quic::ChannelRequest;
use fr_wire::{attachment::Ticket, decoder::Binding, negotiation::ControlBinding};

async fn setup_download(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        ClipboardMode::FileDownload,
    ))
    .await
}
fn request(state: &Fixture) -> ChannelRequest {
    ChannelRequest {
        binding: Binding {
            parent: ControlBinding {
                id: 14,
                ..state.viewer.media.as_ref().unwrap().binding().parent
            },
            ..state.viewer.media.as_ref().unwrap().binding()
        },
        ticket: Ticket(814),
        timeout: Duration::from_secs(2),
    }
}
fn selection(source: &Disk, bytes: &[u8], name: &str) -> Selection {
    let mut selection = Selection::new();
    selection.push(source.source(bytes), name).unwrap();
    selection
}
fn offer(state: &mut Fixture, selected: Selection) {
    let request = request(state);
    state
        .host
        .offer_file_download(
            request,
            selected,
            Permission::new(true),
            fr_files::sender::Policy::default(),
            Duration::from_secs(5),
        )
        .unwrap();
}
async fn retire(state: &mut Fixture, cleanup: &Cx) {
    state.viewer.close();
    state.host.close();
    state
        .viewer
        .reap_files(
            cleanup,
            crate::worker::Deadline::after(cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
    state
        .host
        .reap_files(
            cleanup,
            crate::worker::Deadline::after(cleanup, Duration::from_secs(1)).unwrap(),
        )
        .await
        .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn host_downloads_files_empty_file_and_directory_with_ordered_client_receipts() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        let bytes = vec![0x57; 120_007];
        fs::write(source.0.join("data"), &bytes).unwrap();
        fs::write(source.0.join("empty"), []).unwrap();
        fs::create_dir(source.0.join("tree")).unwrap();
        fs::create_dir(source.0.join("tree/nested-dir")).unwrap();
        fs::write(
            source.0.join("tree/nested-dir/payload"),
            b"selected directory",
        )
        .unwrap();
        let mut selected = Selection::new();
        for name in ["data", "empty"] {
            selected
                .push(File::open(source.0.join(name)).unwrap(), name)
                .unwrap();
        }
        selected
            .push_directory(File::open(source.0.join("tree")).unwrap(), "tree")
            .unwrap();
        state
            .viewer
            .expect_file_download(dest.config(), Duration::from_secs(2))
            .unwrap();
        assert!(state.viewer.file_receive_state().is_none());
        offer(&mut state, selected);
        assert!(state.host.file_send_stage().is_none());
        assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
        let lease = state.viewer.input.binding().lease;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 5_000_000;
                let _ = state.viewer.action(key(true)).unwrap();
                let mut released = false;
                loop {
                    assert!(now(&c).unwrap() < until, "bounded download completion");
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    if !released && state.viewer.pending_actions() == 0 {
                        let _ = state.viewer.action(key(false)).unwrap();
                        released = true;
                    }
                    if state.host.file_batch_report().is_some_and(|r| r.complete)
                        && released
                        && state.viewer.pending_actions() == 0
                    {
                        break;
                    }
                }
                let report = state.host.file_batch_report().unwrap();
                assert_eq!((report.started, report.not_started()), (3, 0));
                assert_eq!(report.stop, None);
                assert!(report.receipts().all(|receipt| matches!(
                    receipt.outcome,
                    Outcome::ControllerPublished {
                        publication: Publication::Durable,
                        ..
                    }
                )));
                let received = state.viewer.file_receive_status().unwrap();
                assert_eq!(received.results.len(), 3);
                assert!(!received.results_truncated);
                assert_eq!(
                    received.results.iter().map(|r| r.id).collect::<Vec<_>>(),
                    [1, 2, 3]
                );
                assert!(
                    received
                        .results
                        .iter()
                        .all(|r| matches!(r.outcome, Ok(Completion::Published(_))))
                );
                assert_eq!(fs::read(dest.0.join("data")).unwrap(), bytes);
                assert!(fs::read(dest.0.join("empty")).unwrap().is_empty());
                assert_eq!(
                    fs::read(dest.0.join("tree/nested-dir/payload")).unwrap(),
                    b"selected directory"
                );
                assert!(dest.0.join("tree/nested-dir").is_dir());
                assert_eq!(state.viewer.input.binding().lease, lease);
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                retire(&mut state, &cleanup).await;
                assert_eq!(state.host.file_batch_report(), Some(report));
                let after = state.viewer.file_receive_status().unwrap();
                assert_eq!(after.results, received.results);
                assert_eq!(after.cleanup, Some(Ok(())));
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn early_download_cancel_settles_attachment_without_starting_a_client_disk_worker() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        let control = ReceiveControl::new();
        assert!(control.cancel());
        state
            .viewer
            .expect_file_download_with_control(
                dest.config(),
                control.clone(),
                Duration::from_secs(2),
            )
            .unwrap();
        offer(
            &mut state,
            selection(&source, b"cancelled before receipt", "never"),
        );
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 3_000_000;
                while state.viewer.file_receive_negotiating() || state.host.file_send_negotiating()
                {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                let _ = state.viewer.action(key(true)).unwrap();
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert!(state.viewer.file_receive_state().is_none());
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                let status = control.status();
                assert_eq!(status.absence, Some(Absence::Refused(SendError::Cancelled)));
                assert_eq!(status.cleanup, Some(Ok(())));
                assert!(status.results.is_empty());
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                retire(&mut state, &cleanup).await;
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}

#[test]
fn revoked_client_file_permission_removes_partial_download_and_keeps_input_live() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        let config = dest.config();
        let permission = config.permission.clone();
        state
            .viewer
            .expect_file_download(config, Duration::from_secs(2))
            .unwrap();
        offer(
            &mut state,
            selection(&source, &vec![0x66; 1_000_007], "partial"),
        );
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 3_000_000;
                while state
                    .viewer
                    .file_receive_progress()
                    .is_none_or(|p| p.staged_bytes == 0)
                {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                permission.revoke();
                let _ = state.viewer.action(key(true)).unwrap();
                loop {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    if state.viewer.try_finish_file_receive_cleanup() == Some(Ok(()))
                        && state.viewer.pending_actions() == 0
                    {
                        break;
                    }
                }
                assert!(!dest.0.join("partial").exists());
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                retire(&mut state, &cleanup).await;
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}

#[test]
fn legacy_upload_scope_cannot_authorize_download_or_spend_its_channel() {
    run(|c, h| async move {
        let mut state = Box::pin(setup(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        assert_eq!(
            state
                .viewer
                .expect_file_download(dest.config(), Duration::from_secs(1)),
            Err(SendError::WrongRole)
        );
        assert_eq!(
            state.host.offer_file_download(
                request(&state),
                selection(&source, b"not shared", "refused"),
                Permission::new(true),
                fr_files::sender::Policy::default(),
                Duration::from_secs(1),
            ),
            Err(crate::session_startup::FileReceiveError::NotNegotiated)
        );
        assert!(state.viewer.file_receive_state().is_none());
        assert!(state.host.file_send_stage().is_none());
        assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
