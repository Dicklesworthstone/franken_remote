//! One standing upload beside ordinary downloads on the original controlled
//! session. ATP, files, TLS/UDP and both service loops are real; desktop and
//! input effects remain the enclosing explicit fixture.
use super::*;
use crate::native_files::{Absence, Directory, Limits, ReceiveControl, ReceiveRequest};
use crate::session_startup::viewer::streaming::files::Lane;
use fr_files::sender::batch::Selection;

fn selected(disk: &Disk, name: &str, bytes: &[u8]) -> Selection {
    let path = disk.0.join(name);
    fs::write(&path, bytes).unwrap();
    let mut batch = Selection::new();
    batch.push(File::open(path).unwrap(), name).unwrap();
    batch
}

async fn publications(
    state: &mut Fixture,
    c: &Cx,
    h: &Cx,
    count: u32,
    n: &mut u128,
    t: &mut u128,
) {
    let until = now(c).unwrap() + 5_000_000;
    loop {
        assert!(now(c).unwrap() < until, "bounded standing-job publication");
        drive(state, c, h, n, t).await;
        let host = state.host.file_upload_sync_status();
        let viewer = state.viewer.file_upload_sync_status();
        for status in [host, viewer].into_iter().flatten() {
            assert_eq!(status.failure, None, "{status:?}");
            assert_eq!(status.state.sync_direction, SyncDirection::ControllerToHost);
        }
        if host.is_some_and(|s| {
            s.confirmed_publications >= count && s.rounds_settled >= u64::from(count)
        }) && viewer.is_some_and(|s| {
            s.confirmed_publications >= count && s.rounds_settled >= u64::from(count)
        })
            && state.viewer.pending_actions() == 0
        {
            break;
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn standing_upload_and_download_keep_independent_authority_after_either_direction_stops() {
    for (download_first, cancel_upload) in [(false, false), (true, true)] {
        run(|c, h| async move {
            let cleanup = Cx::current().unwrap();
            let mut state =
                Box::pin(setup_upload(&c, &h, ClipboardMode::FolderSyncUploadDuplex)).await;
            let source = Disk::new();
            let host_source = Disk::new();
            let host_dest = Disk::new();
            let viewer_dest = Disk::new();
            fs::write(source.0.join("document"), b"controller first").unwrap();
            fs::write(host_dest.0.join("document"), b"host original").unwrap();
            let download = vec![0x87; 1_000_019];
            let mut batch = Some(selected(&host_source, "download-one", &download));
            let mut upload = Some(
                SourceConfig::open(&source.0, 41)
                    .unwrap()
                    .with_scan_interval(Duration::from_millis(100))
                    .unwrap(),
            );
            let receive = ReceiveControl::new();
            for is_download in [download_first, !download_first] {
                if is_download {
                    state
                        .viewer
                        .expect_file_download_with_control(
                            viewer_dest.config(),
                            receive.clone(),
                            Duration::from_secs(2),
                        )
                        .unwrap();
                    let mut download_request = request(&state);
                    download_request.binding.parent.id = 15;
                    download_request.ticket = Ticket(815);
                    state
                        .host
                        .offer_file_download(
                            download_request,
                            batch.take().unwrap(),
                            Permission::new(true),
                            fr_files::sender::Policy::default(),
                            Duration::from_secs(5),
                        )
                        .unwrap();
                } else {
                    state
                        .viewer
                        .expect_folder_upload_source(
                            &mut upload,
                            SendControl::new(),
                            Duration::from_secs(2),
                        )
                        .unwrap();
                    state
                        .host
                        .offer_folder_upload(request(&state), destination(&host_dest), 41)
                        .unwrap();
                }
            }
            assert!(upload.is_none() && batch.is_none());
            assert!(
                state.viewer.file_send_negotiating() && state.viewer.file_receive_negotiating()
            );
            let lease = state.viewer.input.binding().lease;
            let connection = state.viewer.session.transport.binding();
            let driver = state.driver.take().unwrap();
            let ((), done) = Box::pin(support::both(
                async {
                    let (mut n, mut t) = (80_000, 90_000);
                    let _ = state.viewer.action(key(true)).unwrap();
                    publications(&mut state, &c, &h, 1, &mut n, &mut t).await;
                    let until = now(&c).unwrap() + 5_000_000;
                    while state.host.file_batch_report().is_none_or(|r| !r.complete)
                        || state.viewer.pending_actions() != 0
                    {
                        assert!(now(&c).unwrap() < until, "bounded first download");
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                    }
                    let first = state.host.take_file_batch_report().unwrap();
                    assert_eq!((first.started, first.stop), (1, None));
                    assert!(first.receipts().all(|receipt| receipt.outcome
                        == Outcome::ControllerPublished {
                            bytes: download.len() as u64,
                            publication: Publication::Durable,
                        }));
                    assert_eq!(fs::read(viewer_dest.0.join("download-one")).unwrap(), download);
                    assert_eq!(receive.status().results.len(), 1);
                    let original_receipt = receive.status().results[0];
                    fs::write(source.0.join("document"), b"controller second").unwrap();
                    let _ = state.viewer.action(key(false)).unwrap();
                    publications(&mut state, &c, &h, 2, &mut n, &mut t).await;
                    assert_eq!(
                        state.viewer.send_file(
                            File::open(source.0.join("document")).unwrap(),
                            "unrelated"
                        ),
                        Err(SendError::Busy)
                    );
                    let _ = state.viewer.action(key(true)).unwrap();
                    if cancel_upload {
                        state.viewer.cancel_file_send().unwrap();
                        fs::write(source.0.join("document"), b"cancelled source version").unwrap();
                        state
                            .host
                            .send_download_files(
                                selected(
                                    &host_source,
                                    "download-two",
                                    b"independent second download",
                                ),
                                Duration::from_secs(5),
                            )
                            .unwrap();
                        let until = now(&c).unwrap() + 5_000_000;
                        loop {
                            assert!(now(&c).unwrap() < until, "download survives stopped upload");
                            drive(&mut state, &c, &h, &mut n, &mut t).await;
                            if state.host.file_batch_report().is_some_and(|r| r.complete)
                                && state.viewer.try_finish_file_send_cleanup().is_some()
                                && state.host.file_receive_cleanup()
                                    != crate::session_startup::FileReceiveCleanup::Pending
                                && state.viewer.pending_actions() == 0
                            {
                                break;
                            }
                        }
                        assert_eq!(state.viewer.try_finish_file_send_cleanup(), Some(Ok(())));
                        assert_eq!(
                            state.host.file_receive_cleanup(),
                            crate::session_startup::FileReceiveCleanup::Finished(Ok(()))
                        );
                        assert_eq!(
                            fs::read(viewer_dest.0.join("download-two")).unwrap(),
                            b"independent second download"
                        );
                        assert_eq!(receive.status().results.len(), 2);
                        assert_eq!(state.host.file_batch_report().unwrap().stop, None);
                        assert_eq!(
                            state
                                .viewer
                                .file_upload_sync_status()
                                .unwrap()
                                .confirmed_publications,
                            2
                        );
                    } else {
                        assert!(receive.cancel());
                        fs::write(source.0.join("document"), b"controller third").unwrap();
                        publications(&mut state, &c, &h, 3, &mut n, &mut t).await;
                        let until = now(&c).unwrap() + 5_000_000;
                        while state.viewer.try_finish_file_receive_cleanup().is_none() {
                            assert!(now(&c).unwrap() < until, "original download cleanup");
                            drive(&mut state, &c, &h, &mut n, &mut t).await;
                        }
                        assert_eq!(
                            state.viewer.try_finish_file_receive_cleanup(),
                            Some(Ok(()))
                        );
                        assert_eq!(receive.status().results.len(), 1);
                    }
                    assert_eq!(receive.status().results[0], original_receipt);
                    let files: Vec<Vec<u8>> = fs::read_dir(&host_dest.0)
                        .unwrap()
                        .map(|entry| fs::read(entry.unwrap().path()).unwrap())
                        .collect();
                    for expected in [
                        b"host original".as_slice(),
                        b"controller first",
                        b"controller second",
                    ] {
                        assert!(files.iter().any(|bytes| bytes == expected));
                    }
                    if cancel_upload {
                        assert_eq!(files.len(), 3);
                    } else {
                        assert_eq!(files.len(), 4);
                        assert!(files.iter().any(|bytes| bytes == b"controller third"));
                    }
                    let _ = state.viewer.action(key(false)).unwrap();
                    let until = now(&c).unwrap() + 1_000_000;
                    while state.viewer.pending_actions() != 0 {
                        assert!(now(&c).unwrap() < until, "original input remains usable");
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                    }
                    assert_eq!(state.viewer.input.binding().lease, lease);
                    assert!(state.viewer.session.transport.is_bound_to(&connection));
                    assert_eq!(state.effects.lock().unwrap().keys, [true, false, true, false]);
                    assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                    super::super::download::retire(&mut state, &cleanup).await;
                    assert_eq!(receive.status().results[0], original_receipt);
                    assert!(
                        state
                            .viewer
                            .file_upload_sync_status()
                            .unwrap()
                            .cleanup_complete
                    );
                },
                driver,
            ))
            .await;
            assert!(done.handoff_safe());
            assert!(!state.seat.is_occupied());
        });
    }
}

#[test]
fn native_file_registration_keeps_one_job_and_each_independent_ordinary_choice() {
    for download_first in [false, true] {
        let source = Disk::new();
        let destination = Disk::new();
        let directory = Directory::open(&destination.0, Limits::default()).unwrap();
        let mut lane = Lane::default();
        let mut upload = Some(SourceConfig::open(&source.0, 41).unwrap());
        let mut receive = Some(ReceiveRequest::new(&directory));
        for is_download in [download_first, !download_first] {
            if is_download {
                lane.add_receive(receive.take().unwrap(), ReceiveControl::new())
                    .unwrap();
            } else {
                lane.add_folder_upload(upload.take().unwrap(), SendControl::new())
                    .unwrap();
            }
        }
        assert!(lane.has_send() && lane.has_receive() && lane.has_folder_upload());
        assert!(!lane.has_receive_job());
        lane.without_control();
        assert!(lane.has_send() && lane.has_receive());
    }
}

#[test]
fn native_file_registration_refuses_two_jobs_even_after_the_first_is_retired() {
    for download_first in [false, true] {
        for retire in [false, true] {
            let source = Disk::new();
            let destination = Disk::new();
            let directory = Directory::open(&destination.0, Limits::default())
                .unwrap()
                .with_conflict_policy(ConflictPolicy::KeepBoth);
            let mut lane = Lane::default();
            let upload = SourceConfig::open(&source.0, 41).unwrap();
            let receive = ReceiveRequest::new(&directory).with_sync_job(42).unwrap();
            if download_first {
                lane.add_receive(receive, ReceiveControl::new()).unwrap();
                if retire {
                    lane.without_control();
                }
                assert_eq!(
                    lane.add_folder_upload(upload, SendControl::new()),
                    Err(Absence::Unavailable)
                );
            } else {
                lane.add_folder_upload(upload, SendControl::new()).unwrap();
                if retire {
                    lane.without_control();
                }
                assert_eq!(
                    lane.add_receive(receive, ReceiveControl::new()),
                    Err(Absence::Unavailable)
                );
            }
        }
    }
}
