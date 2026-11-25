//! Both production controller loops own the standing job and its ATP lane.
use super::*;
use fr_files::{receive::ConflictPolicy, sync::HostConfig};
use fr_wire::sync_job_state::SyncPhase;

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
            "production folder-sync round stalled: host={:?}, client={:?}",
            state.host.file_sync_status(),
            state.viewer.file_sync_status()
        );
        drive(state, c, h, n, t).await;
        for status in [
            state.host.file_sync_status(),
            state.viewer.file_sync_status(),
        ]
        .into_iter()
        .flatten()
        {
            assert_eq!(status.failure, None, "standing job refused: {status:?}");
        }
        if state
            .host
            .file_sync_status()
            .is_some_and(|status| status.rounds_settled >= round)
            && state
                .viewer
                .file_sync_status()
                .is_some_and(|status| status.rounds_settled >= round)
            && state.viewer.pending_actions() == 0
        {
            break;
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn production_control_loops_deliver_two_folder_rounds_while_original_input_continues() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(fixture_with_clipboard(
            &c,
            &h,
            caps(),
            false,
            false,
            false,
            ClipboardMode::FolderSync,
        ))
        .await;
        let source = Disk::new();
        let destination = Disk::new();
        fs::write(source.0.join("document.txt"), b"host first version").unwrap();
        fs::write(destination.0.join("document.txt"), b"client original").unwrap();
        let mut config = destination.config();
        config.directory = config
            .directory
            .with_conflict_policy(ConflictPolicy::KeepBoth);
        state
            .viewer
            .expect_file_download_with_sync(
                config,
                ReceiveControl::new(),
                Some((41, fr_wire::sync_job_state::JobMode::Flat)),
                Duration::from_secs(2),
            )
            .unwrap();
        state
            .host
            .offer_folder_sync(
                request(&state),
                HostConfig::open(&source.0, 41)
                    .unwrap()
                    .with_scan_interval(Duration::from_millis(100))
                    .unwrap(),
            )
            .unwrap();
        let original_lease = state.viewer.input.binding().lease;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let _ = state.viewer.action(key(true)).unwrap();
                wait_for_round(&mut state, &c, &h, 1, &mut n, &mut t).await;
                assert_eq!(
                    state.host.file_sync_status().unwrap().state.phase,
                    SyncPhase::ConflictRecorded
                );
                assert_eq!(
                    state
                        .host
                        .file_sync_status()
                        .unwrap()
                        .confirmed_publications,
                    1
                );
                assert_eq!(
                    fs::read(destination.0.join("document.txt")).unwrap(),
                    b"client original"
                );
                let _ = state.viewer.action(key(false)).unwrap();
                fs::write(source.0.join("document.txt"), b"host second version").unwrap();
                fs::write(source.0.join("new-file"), b"new source entry").unwrap();
                wait_for_round(&mut state, &c, &h, 2, &mut n, &mut t).await;
                let host = state.host.file_sync_status().unwrap();
                let viewer = state.viewer.file_sync_status().unwrap();
                assert_eq!(host.state, viewer.state);
                assert_eq!(host.state.entries_total, 2);
                assert_eq!(host.state.entries_completed, 2);
                assert_eq!(host.state.conflicts_kept, 1);
                assert_eq!(host.confirmed_publications, 3);
                assert_eq!(viewer.confirmed_publications, 3);
                assert!(state.host.file_batch_report().is_none());
                let received = state.viewer.file_receive_status().unwrap();
                assert_eq!(received.folder_job, Some(viewer));
                assert_eq!(
                    received
                        .results
                        .iter()
                        .map(|result| result.id)
                        .collect::<Vec<_>>(),
                    [1, 2, 3]
                );
                assert!(
                    received
                        .results
                        .iter()
                        .all(|result| matches!(result.outcome, Ok(Completion::Published(_))))
                );
                let files: Vec<Vec<u8>> = fs::read_dir(&destination.0)
                    .unwrap()
                    .map(|entry| fs::read(entry.unwrap().path()).unwrap())
                    .collect();
                for content in [
                    b"client original".as_slice(),
                    b"host first version",
                    b"host second version",
                    b"new source entry",
                ] {
                    assert!(files.iter().any(|bytes| bytes == content));
                }
                assert_eq!(files.len(), 4);
                assert_eq!(state.viewer.input.binding().lease, original_lease);
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                retire(&mut state, &cleanup).await;
                for status in [
                    state.host.file_sync_status().unwrap(),
                    state.viewer.file_sync_status().unwrap(),
                ] {
                    assert_eq!(status.confirmed_publications, 3);
                    assert_eq!(status.rounds_settled, 2);
                    assert!(status.cleanup_complete);
                }
                let final_receive = state.viewer.file_receive_status().unwrap();
                assert_eq!(final_receive.folder_job, state.viewer.file_sync_status());
                assert_eq!(final_receive.cleanup, Some(Ok(())));
                assert_eq!(final_receive.results, received.results);
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert!(!state.seat.is_occupied());
    });
}

#[test]
fn ordinary_download_selection_cannot_enable_a_standing_job() {
    run(|c, h| async move {
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let destination = Disk::new();
        let mut config = destination.config();
        config.directory = config
            .directory
            .with_conflict_policy(ConflictPolicy::KeepBoth);
        assert_eq!(
            state.viewer.expect_file_download_with_sync(
                config,
                ReceiveControl::new(),
                Some((41, fr_wire::sync_job_state::JobMode::Flat)),
                Duration::from_secs(1),
            ),
            Err(SendError::WrongRole)
        );
        assert_eq!(
            state
                .host
                .offer_folder_sync(request(&state), HostConfig::open(&source.0, 41).unwrap(),),
            Err(crate::session_startup::FileReceiveError::NotNegotiated)
        );
        assert!(!state.viewer.file_receive_negotiating());
        assert!(!state.host.file_send_negotiating());
        assert!(state.viewer.file_sync_status().is_none());
        assert!(state.host.file_sync_status().is_none());
        assert_eq!(fs::read_dir(&destination.0).unwrap().count(), 0);
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn recursive_download_refuses_flat_consent_then_publishes_the_selected_subtree() {
    use fr_wire::sync_job_state::JobMode;
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(fixture_with_clipboard(
            &c, &h, caps(), false, false, false, ClipboardMode::RecursiveFolderSync,
        )).await;
        let source = Disk::new();
        let destination = Disk::new();
        let tree = source.0.join("project");
        fs::create_dir_all(tree.join("empty")).unwrap();
        fs::write(tree.join("notes"), b"nested contents").unwrap();
        let target = || {
            let mut config = destination.config();
            config.directory = config.directory.with_conflict_policy(ConflictPolicy::KeepBoth);
            config
        };
        let timeout = Duration::from_secs(2);
        assert_eq!(
            state.viewer.expect_file_download_with_sync(
                target(), ReceiveControl::new(), Some((41, JobMode::Flat)), timeout,
            ),
            Err(SendError::WrongRole)
        );
        assert_eq!(
            state.host.offer_folder_sync(request(&state), HostConfig::open(&source.0, 41).unwrap()),
            Err(crate::session_startup::FileReceiveError::NotNegotiated)
        );
        state.viewer.expect_file_download_with_sync(
            target(), ReceiveControl::new(), Some((41, JobMode::Recursive)), timeout,
        ).unwrap();
        state.host.offer_folder_sync(
            request(&state), HostConfig::open(&source.0, 41).unwrap().with_recursive(),
        ).unwrap();
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                wait_for_round(&mut state, &c, &h, 1, &mut n, &mut t).await;
                assert_eq!(state.viewer.file_sync_status().unwrap().confirmed_publications, 1);
                assert_eq!(fs::read(destination.0.join("project/notes")).unwrap(), b"nested contents");
                assert!(destination.0.join("project/empty").is_dir());
                retire(&mut state, &cleanup).await;
            },
            driver,
        )).await;
        assert!(done.handoff_safe());
    });
}
