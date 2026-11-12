//! Idle attachment and repeated explicit HOST selections through the production
//! application owner, both controller loops, real TLS/UDP, ATP and disk workers.
use super::*;
use crate::{
    native_files::{HostSelection, HostSendIntake, Selection as LocalSelection, SendPhase},
    session_startup::running::streaming::files::Lane as HostLane,
};

fn interactive(h: &Cx) -> (HostLane, HostSendIntake) {
    let (selection, intake) = HostSelection::interactive();
    let mut lane = HostLane::default();
    lane.add_host_download(selection.take(h.clone()).unwrap());
    (lane, intake)
}
fn selected(source: &Disk, name: &str) -> crate::native_files::SendRequest {
    LocalSelection::select(&[source.0.join(name)])
        .unwrap()
        .request()
        .unwrap()
}

#[test]
#[allow(clippy::too_many_lines)]
fn idle_host_lane_sends_later_batches_without_replaying_handles_or_extending_a_deadline() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        let first_bytes = vec![0x65; 120_007];
        fs::write(source.0.join("first"), &first_bytes).unwrap();
        fs::create_dir_all(source.0.join("tree/nested")).unwrap();
        fs::write(source.0.join("tree/nested/data"), b"later host selection").unwrap();
        fs::write(source.0.join("late"), b"must never start").unwrap();
        state
            .viewer
            .expect_file_download(dest.config(), Duration::from_secs(2))
            .unwrap();
        let (mut lane, intake) = interactive(&h);
        let view = state.host_media.binding();
        let lease = state.viewer.input.binding().lease;
        let mut next = 720_000_u128;
        let mut tickets = || nonce(&mut next);
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 5_000_000;
                while !intake.ready() {
                    assert!(now(&c).unwrap() < until, "bounded idle attachment");
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                }
                assert_eq!(state.host.file_send_stage(), Some(Stage::Idle));
                assert!(state.host.file_batch_report().is_none());
                assert_eq!(state.viewer.file_receive_status().unwrap().results.len(), 0);
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                // Control remains usable while the download attachment has no source.
                for pressed in [true, false] {
                    let _ = state.viewer.action(key(pressed)).unwrap();
                    while state.viewer.pending_actions() != 0 {
                        assert!(now(&c).unwrap() < until);
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                        lane.host(&mut state.host, view, &mut tickets);
                    }
                }
                let mut previous = None;
                for (id, name) in [(1, "first"), (2, "tree")] {
                    let control = intake.submit(selected(&source, name)).unwrap();
                    assert_eq!(control.snapshot().cleanup, None);
                    assert!(matches!(
                        intake.submit(selected(&source, "late")),
                        Err(SendError::Busy)
                    ));
                    if let Some((old, status)) = &previous {
                        let old: &crate::native_files::SendControl = old;
                        assert!(!old.cancel() && !old.pause() && !old.resume());
                        assert_eq!(old.status(), *status);
                        assert!(!control.snapshot().cancel_requested);
                    }
                    while !intake.ready() {
                        assert!(now(&c).unwrap() < until, "{:?}", control.snapshot());
                        lane.host(&mut state.host, view, &mut tickets);
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                        lane.host(&mut state.host, view, &mut tickets);
                    }
                    let status = control.status();
                    assert_eq!(
                        control.snapshot().cleanup,
                        None,
                        "an idle reusable sender is not a stopped owner"
                    );
                    assert_eq!(status.phase, SendPhase::Finished);
                    let report = status.report.unwrap();
                    assert!(report.complete && report.stop.is_none());
                    let receipt = report.receipts().next().unwrap();
                    assert_eq!(receipt.id, id);
                    assert!(matches!(
                        receipt.outcome,
                        Outcome::ControllerPublished {
                            publication: Publication::Durable,
                            ..
                        }
                    ));
                    assert_eq!(state.viewer.input.binding().lease, lease);
                    assert!(!state.host.file_send_negotiating());
                    if let Some((old, original)) = &previous {
                        assert_eq!(old.status(), *original);
                    }
                    previous = Some((control, status));
                }
                assert_eq!(fs::read(dest.0.join("first")).unwrap(), first_bytes);
                assert_eq!(
                    fs::read(dest.0.join("tree/nested/data")).unwrap(),
                    b"later host selection"
                );
                let received = state.viewer.file_receive_status().unwrap();
                assert_eq!(
                    received.results.iter().map(|r| r.id).collect::<Vec<_>>(),
                    [1, 2]
                );

                let mut request = selected(&source, "late");
                // Admitted, then expired while queued: the intake refuses a
                // request already past its deadline at submit (6cc7114c).
                request.lifetime = Duration::from_millis(10);
                let expired = intake.submit(request).unwrap();
                asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
                    (now(&h).unwrap() + 20_000) * 1000,
                ))
                .await;
                lane.host(&mut state.host, view, &mut tickets);
                assert_eq!(
                    expired.status().absence,
                    Some(Absence::Refused(SendError::Expired))
                );
                assert_eq!(expired.snapshot().cleanup, Some(Ok(())));
                assert!(intake.is_closed());
                assert!(!dest.0.join("late").exists());
                let (old, original) = previous.unwrap();
                assert_eq!(old.status(), original);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                let _ = state.viewer.action(key(true)).unwrap();
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                retire(&mut state, &cleanup).await;
                lane.close();
                lane.observe(&mut state.host);
                assert_eq!(old.status(), original);
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert_eq!(
            state.effects.lock().unwrap().keys,
            [true, false, true, false]
        );
    });
}

#[test]
fn repeated_host_batches_do_not_reset_the_original_recipient_byte_quota() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        fs::write(source.0.join("first"), b"1234").unwrap();
        fs::write(source.0.join("second"), b"5678").unwrap();
        let mut configuration = dest.config();
        configuration.policy.max_session_bytes = 6;
        state
            .viewer
            .expect_file_download(configuration, Duration::from_secs(2))
            .unwrap();
        let (mut lane, intake) = interactive(&h);
        let view = state.host_media.binding();
        let mut next = 730_000_u128;
        let mut tickets = || nonce(&mut next);
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 4_000_000;
                while !intake.ready() {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                }
                let first = intake.submit(selected(&source, "first")).unwrap();
                while !intake.ready() {
                    assert!(now(&c).unwrap() < until, "{:?}", first.snapshot());
                    lane.host(&mut state.host, view, &mut tickets);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                }
                let original = first.status();
                let second = intake.submit(selected(&source, "second")).unwrap();
                while !intake.is_closed() {
                    assert!(now(&c).unwrap() < until, "{:?}", second.snapshot());
                    lane.host(&mut state.host, view, &mut tickets);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                }
                let report = second.status().report.unwrap();
                assert!(report.complete && report.stop.is_some());
                let receipt = report.receipts().next().unwrap();
                assert_eq!(receipt.id, 2);
                assert_eq!(
                    receipt.outcome,
                    Outcome::ControllerRefused(fr_wire::files::Reason::Resource)
                );
                assert_eq!(fs::read(dest.0.join("first")).unwrap(), b"1234");
                assert!(!dest.0.join("second").exists());
                assert_eq!(first.status(), original);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                retire(&mut state, &cleanup).await;
                lane.close();
                lane.observe(&mut state.host);
                assert_eq!(first.status(), original);
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}

#[test]
fn closing_the_host_command_surface_retires_queued_selection_without_a_source_or_successor() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        fs::write(source.0.join("queued"), b"never admitted").unwrap();
        state
            .viewer
            .expect_file_download(dest.config(), Duration::from_secs(2))
            .unwrap();
        let (selection, intake) = HostSelection::interactive();
        let retained_selector = selection.clone();
        let mut lane = HostLane::default();
        lane.add_host_download(selection.take(h.clone()).unwrap());
        let view = state.host_media.binding();
        let mut next = 740_000_u128;
        let mut tickets = || nonce(&mut next);
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 3_000_000;
                while !intake.ready() {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                }
                let queued = intake.submit(selected(&source, "queued")).unwrap();
                let late_selector = selected(&source, "queued");
                intake.close();
                assert!(intake.is_closed() && !intake.ready());
                assert!(matches!(
                    intake.submit(late_selector),
                    Err(SendError::Closed)
                ));
                assert!(matches!(
                    retained_selector.take(h.clone()),
                    Err(Absence::Unavailable)
                ));
                assert_eq!(
                    queued.status().absence,
                    Some(Absence::Refused(SendError::Cancelled))
                );
                assert_eq!(queued.snapshot().cleanup, Some(Ok(())));
                lane.host(&mut state.host, view, &mut tickets);
                assert_eq!(state.host.file_send_stage(), Some(Stage::Idle));
                assert!(state.host.file_batch_report().is_none());
                let _ = state.viewer.action(key(true)).unwrap();
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                }
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert_eq!(state.viewer.file_receive_status().unwrap().results.len(), 0);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                retire(&mut state, &cleanup).await;
                lane.close();
                lane.observe(&mut state.host);
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
    });
}

#[test]
fn closing_an_active_host_selection_waits_for_real_source_cleanup_and_keeps_input_live() {
    run(|c, h| async move {
        let cleanup = Cx::current().unwrap();
        let mut state = Box::pin(setup_download(&c, &h)).await;
        let source = Disk::new();
        let dest = Disk::new();
        fs::write(source.0.join("partial"), vec![0x4a; 2_000_000]).unwrap();
        state
            .viewer
            .expect_file_download(dest.config(), Duration::from_secs(2))
            .unwrap();
        let (mut lane, intake) = interactive(&h);
        let view = state.host_media.binding();
        let mut next = 750_000_u128;
        let mut tickets = || nonce(&mut next);
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 4_000_000;
                while !intake.ready() {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                }
                let active = intake.submit(selected(&source, "partial")).unwrap();
                loop {
                    assert!(now(&c).unwrap() < until, "{:?}", active.snapshot());
                    lane.host(&mut state.host, view, &mut tickets);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                    assert_eq!(active.snapshot().cleanup, None);
                    if state
                        .viewer
                        .file_receive_progress()
                        .is_some_and(|p| p.staged_bytes > 0 && p.staged_bytes < p.total_bytes)
                    {
                        break;
                    }
                }
                intake.close();
                assert!(active.snapshot().cancel_requested);
                assert_eq!(
                    active.snapshot().cleanup,
                    None,
                    "stop alone never proves source cleanup"
                );
                loop {
                    assert!(now(&c).unwrap() < until, "{:?}", active.snapshot());
                    lane.host(&mut state.host, view, &mut tickets);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    lane.host(&mut state.host, view, &mut tickets);
                    if active.snapshot().cleanup == Some(Ok(()))
                        && state.viewer.try_finish_file_receive_cleanup() == Some(Ok(()))
                    {
                        break;
                    }
                }
                assert!(intake.is_closed());
                assert!(!dest.0.join("partial").exists());
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                let _ = state.viewer.action(key(true)).unwrap();
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                retire(&mut state, &cleanup).await;
                lane.close();
                lane.observe(&mut state.host);
                assert_eq!(active.snapshot().cleanup, Some(Ok(())));
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
    });
}
