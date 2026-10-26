//! Two explicit selections through the production application owners and SAME
//! controller, completed file channel, ATP sequence domain and disk destination.
use super::*;
use crate::native_files::SendIntake;

#[test]
#[allow(clippy::too_many_lines)]
fn a_later_selection_keeps_the_original_lane_and_prior_result_handles() {
    run(|c, h| async move {
        let mut state = drop_fixture(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        fs::write(source.0.join("first"), b"first selected bytes").unwrap();
        fs::write(source.0.join("second"), b"second selected bytes").unwrap();
        fs::write(source.0.join("late"), b"must never start").unwrap();
        let directory = Directory::open(&dest.0, Limits::default()).unwrap();
        let initial = Selection::select(&[source.0.join("first")])
            .unwrap()
            .request()
            .unwrap();
        let intake = SendIntake::new(c.clone(), initial.policy);
        let first = SendControl::new();
        let mut viewer = ViewerLane::default();
        viewer.add_send(initial, first.clone()).unwrap();
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
                    assert!(now(&c).unwrap() < until, "{:?}", first.status());
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    host.host(&mut state.host, view, &mut ticket);
                    viewer.viewer(&mut state.viewer);
                    if intake.ready() {
                        break;
                    }
                }
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
        assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
    });
}
