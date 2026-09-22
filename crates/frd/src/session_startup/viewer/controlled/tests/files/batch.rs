//! Original controller, real TLS/UDP/ATP, disk publication and input renewal.
use super::*;
use fr_files::sender::batch::{Report, Selection, Stop};

fn selected(source: &Disk, names: &[&str]) -> Selection {
    let mut selection = Selection::new();
    for name in names {
        selection
            .push(File::open(source.0.join("source")).unwrap(), name)
            .unwrap();
    }
    selection
}
async fn finish_batch(state: &mut Fixture, c: &Cx, h: &Cx) -> Report {
    let (mut n, mut t) = (80000, 90000);
    let until = now(c).unwrap() + 2_000_000;
    loop {
        assert!(now(c).unwrap() < until, "bounded batch completion");
        drive(state, c, h, &mut n, &mut t).await;
        let report = state.viewer.file_batch_report().unwrap();
        if report.complete && state.viewer.pending_actions() == 0 {
            return report;
        }
    }
}
#[test]
#[allow(clippy::too_many_lines)]
fn multi_selection_publishes_in_order_without_interleaving_or_losing_input() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        drop(source.source(&vec![0x41; 80_007]));
        fs::write(source.0.join("empty"), []).unwrap();
        let mut selection = selected(&source, &["first", "second"]);
        selection
            .push(File::open(source.0.join("empty")).unwrap(), "empty")
            .unwrap();
        join(&mut state, &dest, Permission::new(true));
        let lease = state.viewer.input.binding().lease;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                state
                    .viewer
                    .send_files(selection, Duration::from_secs(2))
                    .unwrap();
                assert_eq!(state.viewer.file_stage(), Some(Stage::Queued));
                assert_eq!(state.viewer.file_batch_report().unwrap().started, 0);
                assert!(state.viewer.take_file_batch_report().is_none());
                assert_eq!(
                    state
                        .viewer
                        .send_file(File::open(source.0.join("empty")).unwrap(), "no-interleave"),
                    Err(SendError::Busy)
                );
                assert_eq!(
                    state
                        .viewer
                        .send_files(selected(&source, &["no-overlap"]), Duration::from_secs(2)),
                    Err(SendError::Busy)
                );
                let _ = state.viewer.action(key(true)).unwrap();
                let report = finish_batch(&mut state, &c, &h).await;
                assert_eq!(
                    (report.total, report.started, report.not_started()),
                    (3, 3, 0)
                );
                assert_eq!(report.stop, None);
                for (index, receipt) in report.receipts().enumerate() {
                    assert_eq!(receipt.id, index as u64 + 1);
                    assert_eq!(
                        receipt.outcome,
                        Outcome::HostPublished {
                            bytes: if index == 2 { 0 } else { 80_007 },
                            publication: Publication::Durable,
                        }
                    );
                }
                for name in ["first", "second"] {
                    assert_eq!(fs::read(dest.0.join(name)).unwrap(), vec![0x41; 80_007]);
                }
                assert_eq!(fs::metadata(dest.0.join("empty")).unwrap().len(), 0);
                assert_eq!(state.viewer.input.binding().lease, lease);
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                assert!(state.viewer.take_file_result().is_none());
                assert_eq!(state.viewer.take_file_batch_report(), Some(report));
                assert_eq!(state.viewer.file_batch_report(), None);
                assert_eq!(
                    state
                        .viewer
                        .send_file(File::open(source.0.join("empty")).unwrap(), "after"),
                    Ok(4)
                );
                state.viewer.cancel_files().unwrap();
                state
                    .viewer
                    .reap_files(
                        &h,
                        crate::worker::Deadline::after(&h, Duration::from_secs(1)).unwrap(),
                    )
                    .await
                    .unwrap();
                assert!(state.viewer.take_file_result().is_some());
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}
#[test]
fn conflict_stops_later_sources_and_retains_earlier_durable_receipts() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        drop(source.source(b"new"));
        fs::write(dest.0.join("conflict"), b"existing").unwrap();
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                state
                    .viewer
                    .send_files(
                        selected(&source, &["first", "conflict", "never"]),
                        Duration::from_secs(2),
                    )
                    .unwrap();
                let _ = state.viewer.action(key(true)).unwrap();
                let report = finish_batch(&mut state, &c, &h).await;
                assert_eq!(
                    report.stop,
                    Some(Stop::HostRefused(fr_wire::files::Reason::Conflict))
                );
                assert_eq!(
                    (
                        report.started,
                        report.not_started(),
                        report.receipts().len()
                    ),
                    (2, 1, 2)
                );
                assert!(matches!(
                    report.receipts().next().unwrap().outcome,
                    Outcome::HostPublished {
                        publication: Publication::Durable,
                        ..
                    }
                ));
                assert_eq!(fs::read(dest.0.join("first")).unwrap(), b"new");
                assert_eq!(fs::read(dest.0.join("conflict")).unwrap(), b"existing");
                assert!(!dest.0.join("never").exists());
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                state.viewer.close();
                assert_eq!(state.viewer.file_batch_report(), Some(report));
                assert_eq!(state.viewer.take_file_batch_report(), Some(report));
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}
#[test]
fn revoked_selection_never_opens_a_source_and_report_survives_streaming_close() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture_with_clipboard(
            &c,
            &h,
            caps(),
            true,
            false,
            false,
            ClipboardMode::Files,
        ))
        .await;
        let source = Disk::new();
        let dest = Disk::new();
        drop(source.source(b"must-not-send"));
        let permission = Permission::new(true);
        join(&mut state, &dest, permission.clone());
        state
            .viewer
            .send_files(
                selected(&source, &["first", "second"]),
                Duration::from_secs(1),
            )
            .unwrap();
        permission.revoke();
        assert_eq!(
            state
                .viewer
                .send_files(selected(&source, &["not-admitted"]), Duration::from_secs(1)),
            Err(SendError::Cancelled)
        );
        let report = state.viewer.file_batch_report().unwrap();
        assert_eq!(
            (
                report.started,
                report.not_started(),
                report.receipts().len()
            ),
            (0, 2, 0)
        );
        assert_eq!(report.stop, Some(Stop::Local(SendError::Cancelled)));
        assert!(report.complete);
        let mut streaming = state
            .viewer
            .into_streaming(state.presenter.take().unwrap(), state.receiver)
            .unwrap();
        streaming
            .reap_files(
                &h,
                crate::worker::Deadline::after(&h, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(streaming.file_batch_report(), Some(report));
        assert_eq!(streaming.take_file_batch_report(), Some(report));
        assert_eq!(streaming.take_file_batch_report(), None);
        assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
        streaming
            .reap_media(
                &h,
                crate::worker::Deadline::after(&h, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}

#[test]
fn locally_selected_keep_both_allows_the_remaining_batch_to_finish() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        drop(source.source(b"incoming"));
        fs::write(dest.0.join("report.txt"), b"existing").unwrap();
        let mut config = dest.config();
        config.directory = config
            .directory
            .with_conflict_policy(fr_files::receive::ConflictPolicy::KeepBoth);
        join_with_config(
            &mut state,
            config,
            Permission::new(true),
            fr_files::sender::Policy::default(),
        );
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                state
                    .viewer
                    .send_files(
                        selected(&source, &["report.txt", "next.txt"]),
                        Duration::from_secs(2),
                    )
                    .unwrap();
                let report = finish_batch(&mut state, &c, &h).await;
                assert_eq!((report.started, report.not_started()), (2, 0));
                assert_eq!(report.stop, None);
                assert_eq!(report.receipts().len(), 2);
                assert!(report.receipts().all(|r| r.outcome
                    == Outcome::HostPublished {
                        bytes: 8,
                        publication: Publication::Durable,
                    }));
                assert_eq!(fs::read(dest.0.join("report.txt")).unwrap(), b"existing");
                assert_eq!(fs::read(dest.0.join("next.txt")).unwrap(), b"incoming");
                let names = fs::read_dir(&dest.0)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name())
                    .collect::<Vec<_>>();
                assert_eq!(names.len(), 3);
                let renamed = names
                    .iter()
                    .find(|name| name.to_str().unwrap().starts_with("report.fr-conflict-"))
                    .unwrap();
                assert_eq!(fs::read(dest.0.join(renamed)).unwrap(), b"incoming");
                state.viewer.close();
                assert_eq!(state.viewer.take_file_batch_report(), Some(report));
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}
