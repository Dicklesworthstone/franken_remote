//! Two explicit selections through the production application owners and SAME
//! controller, completed file channel, ATP sequence domain and disk destination.
use super::*;
use crate::native_files::SendIntake;

#[test]
fn a_later_selection_keeps_the_original_lane_and_prior_result_handles() {
    selections_on_the_original_lane(false);
}

#[test]
fn a_first_selection_after_connection_keeps_control_and_accepts_later_batches() {
    selections_on_the_original_lane(true);
}

#[allow(clippy::too_many_lines)]
fn selections_on_the_original_lane(deferred: bool) {
    run(move |c, h| async move {
        let mut state = drop_fixture(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        fs::write(source.0.join("first"), b"first selected bytes").unwrap();
        fs::write(source.0.join("second"), b"second selected bytes").unwrap();
        fs::write(source.0.join("late"), b"must never start").unwrap();
        let directory = Directory::open(&dest.0, Limits::default()).unwrap();
        let intake = SendIntake::new(c.clone(), fr_files::sender::Policy::default());
        let initial = SendControl::new();
        let mut viewer = ViewerLane::default();
        if deferred {
            viewer.add_deferred_send(initial.clone()).unwrap();
        } else {
            let request = Selection::select(&[source.0.join("first")])
                .unwrap()
                .request()
                .unwrap();
            viewer.add_send(request, initial.clone()).unwrap();
        }
        viewer.set_intake(intake.clone());
        let mut host = HostLane::default();
        host.add_upload(&directory);
        let view = state.host_media.binding();
        let lease = state.viewer.input.binding().lease;
        let mut tickets = 700_000_u128;
        let mut ticket = || nonce(&mut tickets);
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 3_000_000;
                loop {
                    assert!(now(&c).unwrap() < until, "{:?}", initial.status());
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    host.host(&mut state.host, view, &mut ticket);
                    viewer.viewer(&mut state.viewer);
                    if intake.ready() {
                        break;
                    }
                }
                let first = if deferred {
                    assert_eq!(initial.status().phase, SendPhase::WaitingForSelection);
                    assert!(initial.status().report.is_none());
                    assert!(state.viewer.file_batch_report().is_none());
                    assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                    assert_eq!(state.viewer.input.binding().lease, lease);
                    // Real control still works while this established optional
                    // lane has never owned a source or created an ATP batch.
                    for pressed in [true, false] {
                        let _ = state.viewer.action(key(pressed)).unwrap();
                        while state.viewer.pending_actions() != 0 {
                            assert!(now(&c).unwrap() < until);
                            drive(&mut state, &c, &h, &mut n, &mut t).await;
                            host.host(&mut state.host, view, &mut ticket);
                            viewer.viewer(&mut state.viewer);
                        }
                    }
                    let selected = intake
                        .submit(
                            Selection::select(&[source.0.join("first")])
                                .unwrap()
                                .request()
                                .unwrap(),
                        )
                        .unwrap();
                    loop {
                        assert!(now(&c).unwrap() < until, "{:?}", selected.status());
                        viewer.viewer(&mut state.viewer);
                        assert!(!state.viewer.file_send_negotiating());
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                        host.host(&mut state.host, view, &mut ticket);
                        viewer.viewer(&mut state.viewer);
                        if intake.ready() {
                            break;
                        }
                    }
                    assert_eq!(initial.status().phase, SendPhase::SelectionAccepted);
                    assert!(initial.status().report.is_none());
                    assert!(!initial.cancel() && !initial.pause() && !initial.resume());
                    assert!(!selected.cancel_requested());
                    selected
                } else {
                    initial
                };
                let original = first.status();
                assert_eq!(original.phase, SendPhase::Finished);
                let first_report = original.report.unwrap();
                assert_eq!(first_report.stop, None);
                assert_eq!(first_report.receipts().next().unwrap().id, 1);
                let second = intake
                    .submit(
                        Selection::select(&[source.0.join("second")])
                            .unwrap()
                            .request()
                            .unwrap(),
                    )
                    .unwrap();
                assert!(!first.cancel() && !first.pause() && !first.resume());
                assert!(!intake.ready());
                loop {
                    assert!(now(&c).unwrap() < until, "{:?}", second.status());
                    viewer.viewer(&mut state.viewer);
                    assert!(!state.viewer.file_send_negotiating());
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    host.host(&mut state.host, view, &mut ticket);
                    viewer.viewer(&mut state.viewer);
                    if intake.ready() {
                        break;
                    }
                }
                let report = second.status().report.unwrap();
                assert!(report.complete);
                assert_eq!(report.stop, None);
                assert_eq!(report.receipts().next().unwrap().id, 2);
                assert_eq!(first.status(), original);
                assert!(!second.cancel_requested());
                assert_eq!(
                    fs::read(dest.0.join("first")).unwrap(),
                    b"first selected bytes"
                );
                assert_eq!(
                    fs::read(dest.0.join("second")).unwrap(),
                    b"second selected bytes"
                );
                assert_eq!(state.viewer.input.binding().lease, lease);
                let mut expired = Selection::select(&[source.0.join("late")])
                    .unwrap()
                    .request()
                    .unwrap();
                expired.lifetime = Duration::from_micros(1);
                let late = intake.submit(expired).unwrap();
                asupersync::time::sleep_until(asupersync::types::Time::from_nanos(
                    (now(&c).unwrap() + 2_000) * 1000,
                ))
                .await;
                viewer.viewer(&mut state.viewer);
                assert_eq!(
                    late.status().absence,
                    Some(Absence::Refused(SendError::Expired))
                );
                assert_eq!(late.snapshot().cleanup, Some(Ok(())));
                assert!(intake.is_closed());
                assert!(!dest.0.join("late").exists());
                assert_eq!(first.status(), original);
                let _ = state.viewer.action(key(true)).unwrap();
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until + 1_000_000);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert!(!state.viewer.is_closed());
                state.viewer.close();
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
        let expected: &[bool] = if deferred {
            &[true, false, true, false]
        } else {
            &[true, false]
        };
        assert_eq!(state.effects.lock().unwrap().keys, expected);
    });
}
