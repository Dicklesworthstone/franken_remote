//! Reverse standing jobs use both production control loops, real TLS/UDP/ATP,
//! and original disk workers. Only desktop/input effects use the enclosing fixture.
use super::*;
use crate::native_files::SendControl;
use fr_files::{receive::ConflictPolicy, sync::SourceConfig};
use fr_transport::quic::ChannelRequest;
use fr_wire::{
    attachment::Ticket,
    decoder::Binding,
    negotiation::ControlBinding,
    sync_job_state::{SyncDirection, SyncPhase},
};

async fn setup_upload(c: &Cx, h: &Cx, mode: ClipboardMode) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        mode,
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

fn destination(disk: &Disk) -> fr_files::quic::Configuration {
    let mut config = disk.config();
    config.directory = config
        .directory
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    config
}

async fn wait_for_round(
    state: &mut Fixture,
    c: &Cx,
    h: &Cx,
    round: u64,
    n: &mut u128,
    t: &mut u128,
) {
    let until = now(c).unwrap() + 5_000_000;
    loop {
        assert!(
            now(c).unwrap() < until,
            "reverse folder round stalled: host={:?}, controller={:?}",
            state.host.file_upload_sync_status(),
            state.viewer.file_upload_sync_status()
        );
        drive(state, c, h, n, t).await;
        for status in [
            state.host.file_upload_sync_status(),
            state.viewer.file_upload_sync_status(),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(status.failure, None, "standing upload refused: {status:?}");
            assert_eq!(status.state.sync_direction, SyncDirection::ControllerToHost);
        }
        if state
            .host
            .file_upload_sync_status()
            .is_some_and(|status| status.rounds_settled >= round)
            && state
                .viewer
                .file_upload_sync_status()
                .is_some_and(|status| status.rounds_settled >= round)
            && state.viewer.pending_actions() == 0
        {
            break;
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn controller_source_delivers_two_rounds_preserving_host_files_and_original_input() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_upload(&c, &h, ClipboardMode::FolderSyncUpload)).await;
        let source = Disk::new();
        let dest = Disk::new();
        fs::write(source.0.join("document.txt"), b"controller first version").unwrap();
        fs::write(dest.0.join("document.txt"), b"host original").unwrap();
        // A caller cannot expand an original flat grant to cover subdirectories.
        let mut tree = Some(SourceConfig::open(&source.0, 41).unwrap().with_recursive());
        assert_eq!(
            state.viewer.expect_folder_upload_source(
                &mut tree,
                SendControl::new(),
                Duration::from_secs(2),
            ),
            Err(SendError::WrongRole)
        );
        assert!(tree.is_some());
        assert_eq!(
            state.host.offer_folder_upload_mode(
                request(&state),
                destination(&dest),
                41,
                fr_wire::sync_job_state::JobMode::Recursive,
            ),
            Err(crate::session_startup::FileReceiveError::NotNegotiated)
        );
        state
            .viewer
            .expect_folder_upload(
                SourceConfig::open(&source.0, 41)
                    .unwrap()
                    .with_scan_interval(Duration::from_millis(100))
                    .unwrap(),
                Duration::from_secs(2),
            )
            .unwrap();
        state
            .host
            .offer_folder_upload(request(&state), destination(&dest), 41)
            .unwrap();
        assert!(state.viewer.file_upload_sync_status().is_none());
        assert!(state.host.file_upload_sync_status().is_none());
        let original_lease = state.viewer.input.binding().lease;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let _ = state.viewer.action(key(true)).unwrap();
                wait_for_round(&mut state, &c, &h, 1, &mut n, &mut t).await;
                let first = state.viewer.file_upload_sync_status().unwrap();
                assert_eq!(first.state.phase, SyncPhase::ConflictRecorded);
                assert_eq!(first.confirmed_publications, 1);
                assert_eq!(first.confirmed_conflicts, 1);
                assert_eq!(
                    fs::read(dest.0.join("document.txt")).unwrap(),
                    b"host original"
                );
                // An ordinary selection cannot steal a standing job's sender,
                // even while the previous round is settled and source work idle.
                assert_eq!(
                    state.viewer.send_file(
                        File::open(source.0.join("document.txt")).unwrap(),
                        "unrelated"
                    ),
                    Err(SendError::Busy)
                );
                assert!(state.viewer.take_file_batch_report().is_none());
                assert!(state.viewer.take_file_result().is_none());
                let _ = state.viewer.action(key(false)).unwrap();
                fs::write(source.0.join("document.txt"), b"controller second version").unwrap();
                fs::write(source.0.join("new-file"), b"new source entry").unwrap();
                wait_for_round(&mut state, &c, &h, 2, &mut n, &mut t).await;
                let host = state.host.file_upload_sync_status().unwrap();
                let controller = state.viewer.file_upload_sync_status().unwrap();
                assert_eq!(host.state, controller.state);
                assert_eq!(controller.state.entries_total, 2);
                assert_eq!(controller.state.entries_completed, 2);
                assert_eq!(controller.state.conflicts_kept, 1);
                assert_eq!(host.confirmed_publications, 3);
                assert_eq!(controller.confirmed_publications, 3);
                assert_eq!(host.confirmed_conflicts, 2);
                assert_eq!(controller.confirmed_conflicts, 2);
                let files: Vec<Vec<u8>> = fs::read_dir(&dest.0)
                    .unwrap()
                    .map(|entry| fs::read(entry.unwrap().path()).unwrap())
                    .collect();
                for content in [
                    b"host original".as_slice(),
                    b"controller first version",
                    b"controller second version",
                    b"new source entry",
                ] {
                    assert!(files.iter().any(|bytes| bytes == content));
                }
                assert_eq!(files.len(), 4);
                assert_eq!(state.viewer.input.binding().lease, original_lease);
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                super::download::retire(&mut state, &cleanup).await;
                for status in [
                    state.host.file_upload_sync_status().unwrap(),
                    state.viewer.file_upload_sync_status().unwrap(),
                ] {
                    assert_eq!(status.confirmed_publications, 3);
                    assert_eq!(status.confirmed_conflicts, 2);
                    assert_eq!(status.rounds_settled, 2);
                    assert!(status.cleanup_complete);
                }
                assert!(state.viewer.file_cleanup_finished());
                assert_eq!(
                    state.host.file_receive_cleanup(),
                    crate::session_startup::FileReceiveCleanup::Finished(Ok(()))
                );
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
#[allow(clippy::too_many_lines)]
fn recursive_upload_preserves_complete_versions_and_original_control_across_rounds() {
    use fr_wire::sync_job_state::JobMode;

    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_upload(
            &c,
            &h,
            ClipboardMode::RecursiveFolderSyncUpload,
        ))
        .await;
        let source = Disk::new();
        let dest = Disk::new();
        fs::create_dir_all(source.0.join("tree/nested")).unwrap();
        fs::create_dir(source.0.join("tree/empty-dir")).unwrap();
        fs::write(source.0.join("tree/empty-file"), []).unwrap();
        fs::write(source.0.join("tree/nested/payload"), b"first tree version").unwrap();
        fs::create_dir(dest.0.join("tree")).unwrap();
        fs::write(dest.0.join("tree/local"), b"host tree").unwrap();

        // Wire version 2 cannot expand either endpoint's local flat choice.
        let mut flat = Some(SourceConfig::open(&source.0, 41).unwrap());
        assert_eq!(
            state.viewer.expect_folder_upload_source(
                &mut flat,
                SendControl::new(),
                Duration::from_secs(2),
            ),
            Err(SendError::WrongRole)
        );
        assert!(flat.is_some());
        assert_eq!(
            state
                .host
                .offer_folder_upload(request(&state), destination(&dest), 41),
            Err(crate::session_startup::FileReceiveError::NotNegotiated)
        );
        assert!(!state.viewer.file_send_negotiating());
        assert!(!state.host.file_receive_negotiating());
        assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 1);

        state
            .viewer
            .expect_folder_upload(
                SourceConfig::open(&source.0, 41)
                    .unwrap()
                    .with_recursive()
                    .with_scan_interval(Duration::from_millis(100))
                    .unwrap(),
                Duration::from_secs(2),
            )
            .unwrap();
        state
            .host
            .offer_folder_upload_mode(request(&state), destination(&dest), 41, JobMode::Recursive)
            .unwrap();
        let lease = state.viewer.input.binding().lease;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let _ = state.viewer.action(key(true)).unwrap();
                wait_for_round(&mut state, &c, &h, 1, &mut n, &mut t).await;
                fs::write(source.0.join("tree/nested/payload"), b"second tree version").unwrap();
                fs::write(
                    source.0.join("tree/nested/added"),
                    b"only in the second version",
                )
                .unwrap();
                let _ = state.viewer.action(key(false)).unwrap();
                wait_for_round(&mut state, &c, &h, 2, &mut n, &mut t).await;

                assert_eq!(fs::read(dest.0.join("tree/local")).unwrap(), b"host tree");
                assert!(!dest.0.join("tree/nested").exists());
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 3);
                let mut versions = Vec::new();
                for entry in fs::read_dir(&dest.0).unwrap() {
                    let root = entry.unwrap().path();
                    if root.join("nested/payload").exists() {
                        let payload = fs::read(root.join("nested/payload")).unwrap();
                        if payload == b"first tree version" {
                            assert!(!root.join("nested/added").exists());
                        } else {
                            assert_eq!(
                                fs::read(root.join("nested/added")).unwrap(),
                                b"only in the second version"
                            );
                        }
                        versions.push(payload);
                        assert!(root.join("empty-dir").is_dir());
                        assert_eq!(fs::metadata(root.join("empty-file")).unwrap().len(), 0);
                        assert!(!root.join("local").exists());
                    }
                }
                versions.sort();
                assert_eq!(
                    versions,
                    [
                        b"first tree version".to_vec(),
                        b"second tree version".to_vec()
                    ]
                );
                assert_eq!(state.viewer.input.binding().lease, lease);
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                super::download::retire(&mut state, &cleanup).await;
                for status in [
                    state.host.file_upload_sync_status().unwrap(),
                    state.viewer.file_upload_sync_status().unwrap(),
                ] {
                    assert_eq!(status.confirmed_publications, 2);
                    assert_eq!(status.confirmed_conflicts, 2);
                    assert_eq!(status.rounds_settled, 2);
                    assert!(status.cleanup_complete);
                }
                assert!(state.viewer.file_cleanup_finished());
                assert_eq!(
                    state.host.file_receive_cleanup(),
                    crate::session_startup::FileReceiveCleanup::Finished(Ok(()))
                );
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn ordinary_upload_capability_cannot_enable_a_standing_job() {
    run(|c, h| async move {
        let mut state = Box::pin(setup_upload(&c, &h, ClipboardMode::FileDrop)).await;
        let source = Disk::new();
        let dest = Disk::new();
        let mut selected = Some(SourceConfig::open(&source.0, 41).unwrap());
        assert_eq!(
            state.viewer.expect_folder_upload_source(
                &mut selected,
                SendControl::new(),
                Duration::from_secs(1),
            ),
            Err(SendError::WrongRole)
        );
        assert!(
            selected.is_some(),
            "failed negotiation must retain local source consent"
        );
        assert_eq!(
            state
                .host
                .offer_folder_upload(request(&state), destination(&dest), 41),
            Err(crate::session_startup::FileReceiveError::NotNegotiated)
        );
        assert!(!state.viewer.file_send_negotiating());
        assert!(!state.host.file_receive_negotiating());
        assert!(state.viewer.file_upload_sync_status().is_none());
        assert!(state.host.file_upload_sync_status().is_none());
        assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn native_duplex_refuses_two_standing_jobs_before_spending_source_or_attachment() {
    for download_first in [false, true] {
        run(|c, h| async move {
            let mut state =
                Box::pin(setup_upload(&c, &h, ClipboardMode::FolderSyncJobsDuplex)).await;
            let source = Disk::new();
            let dest = Disk::new();
            fs::write(source.0.join("selected"), b"explicit local source").unwrap();
            let mut selected = Some(SourceConfig::open(&source.0, 41).unwrap());
            let mut download_request = request(&state);
            download_request.binding.parent.id = 15;
            download_request.ticket = Ticket(815);
            if download_first {
                state
                    .viewer
                    .expect_file_download_with_sync(
                        destination(&dest),
                        crate::native_files::ReceiveControl::new(),
                        Some((42, fr_wire::sync_job_state::JobMode::Flat)),
                        Duration::from_secs(2),
                    )
                    .unwrap();
                state
                    .host
                    .offer_folder_sync(
                        download_request,
                        fr_files::sync::HostConfig::open(&source.0, 42).unwrap(),
                    )
                    .unwrap();
                assert_eq!(
                    state.viewer.expect_folder_upload_source(
                        &mut selected,
                        SendControl::new(),
                        Duration::from_secs(2),
                    ),
                    Err(SendError::Busy)
                );
                assert!(selected.is_some());
                assert_eq!(
                    state
                        .host
                        .offer_folder_upload(request(&state), destination(&dest), 41),
                    Err(crate::session_startup::FileReceiveError::AlreadyAttached)
                );
                assert!(state.viewer.file_receive_negotiating());
                assert!(state.host.file_send_negotiating());
                assert!(!state.viewer.file_send_negotiating());
                assert!(!state.host.file_receive_negotiating());
            } else {
                state
                    .viewer
                    .expect_folder_upload_source(
                        &mut selected,
                        SendControl::new(),
                        Duration::from_secs(2),
                    )
                    .unwrap();
                state
                    .host
                    .offer_folder_upload(request(&state), destination(&dest), 41)
                    .unwrap();
                assert!(selected.is_none());
                assert_eq!(
                    state.viewer.expect_file_download_with_sync(
                        destination(&dest),
                        crate::native_files::ReceiveControl::new(),
                        Some((42, fr_wire::sync_job_state::JobMode::Flat)),
                        Duration::from_secs(2),
                    ),
                    Err(SendError::Busy)
                );
                assert_eq!(
                    state.host.offer_folder_sync(
                        download_request,
                        fr_files::sync::HostConfig::open(&source.0, 42).unwrap(),
                    ),
                    Err(crate::session_startup::FileReceiveError::AlreadyAttached)
                );
                assert!(state.viewer.file_send_negotiating());
                assert!(state.host.file_receive_negotiating());
                assert!(!state.viewer.file_receive_negotiating());
                assert!(!state.host.file_send_negotiating());
            }
            assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
            state.viewer.close();
            state.host.close();
            assert!(state.driver.take().unwrap().await.handoff_safe());
        });
    }
}

#[test]
fn cancelling_before_or_during_attachment_never_starts_a_source_or_ends_input() {
    for cancel_before_expectation in [false, true] {
        run(|c, h| async move {
            let cleanup = Cx::current().unwrap();
            let mut state = Box::pin(setup_upload(&c, &h, ClipboardMode::FolderSyncUpload)).await;
            let source = Disk::new();
            let dest = Disk::new();
            // An actual source scan would fail on this entry. The cancelled
            // original consent must never create a SourceJob or scan worker.
            std::os::unix::fs::symlink("missing", source.0.join("unsupported")).unwrap();
            let control = SendControl::new();
            let mut native = crate::session_startup::viewer::streaming::files::Lane::default();
            native
                .add_folder_upload(SourceConfig::open(&source.0, 41).unwrap(), control.clone())
                .unwrap();
            if cancel_before_expectation {
                assert!(control.cancel());
            }
            native.viewer(&mut state.viewer);
            assert!(state.viewer.file_send_negotiating());
            if !cancel_before_expectation {
                assert!(control.cancel());
            }
            state
                .host
                .offer_folder_upload(request(&state), destination(&dest), 41)
                .unwrap();
            let original_lease = state.viewer.input.binding().lease;
            let driver = state.driver.take().unwrap();
            let ((), done) = Box::pin(support::both(
                async {
                    let (mut n, mut t) = (80_000, 90_000);
                    let until = now(&c).unwrap() + 5_000_000;
                    let _ = state.viewer.action(key(true)).unwrap();
                    let mut released = false;
                    loop {
                        assert!(
                            now(&c).unwrap() < until,
                            "cancelled attachment failed to settle"
                        );
                        native.viewer(&mut state.viewer);
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                        native.viewer(&mut state.viewer);
                        assert!(state.viewer.file_upload_sync_status().is_none());
                        assert!(control.folder_job_status().is_none());
                        if !released && state.viewer.pending_actions() == 0 {
                            let _ = state.viewer.action(key(false)).unwrap();
                            released = true;
                        }
                        if released
                            && state.viewer.pending_actions() == 0
                            && !state.viewer.file_send_negotiating()
                            && !state.host.file_receive_negotiating()
                            && control.snapshot().cleanup == Some(Ok(()))
                            && state.host.file_receive_cleanup()
                                != crate::session_startup::FileReceiveCleanup::Pending
                        {
                            break;
                        }
                    }
                    assert_eq!(
                        control.status().phase,
                        crate::native_files::SendPhase::Ended
                    );
                    assert_eq!(
                        control.status().absence,
                        Some(crate::native_files::Absence::Refused(SendError::Cancelled))
                    );
                    assert!(state.viewer.file_stage().is_none());
                    assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                    assert!(
                        state
                            .host
                            .file_upload_sync_status()
                            .is_none_or(|status| status.confirmed_publications == 0)
                    );
                    assert_eq!(state.viewer.input.binding().lease, original_lease);
                    assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                    assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                    super::download::retire(&mut state, &cleanup).await;
                    native.observe(&state.viewer);
                    assert_eq!(control.snapshot().cleanup, Some(Ok(())));
                },
                driver,
            ))
            .await;
            assert!(done.handoff_safe());
        });
    }
}

mod duplex;
