//! The two `frd run --files` / `fr connect --send` lane owners, through both
//! production controller loops over real UDP/TLS, ATP and a real directory.
//! Consent, pixels and the counted input sink are the fixture's, as above.
use super::*;
use crate::{
    native_files::{Absence, Directory, Limits, Selection, SendControl, SendPhase},
    session_startup::{
        running::streaming::files::Lane as HostLane, viewer::streaming::files::Lane as ViewerLane,
    },
};
use fr_wire::files::Reason;

async fn drop_fixture(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        ClipboardMode::FileDrop,
    ))
    .await
}
fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[3]
        })
        .collect()
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_offer_waits_for_renewal_then_publishes_in_order_and_refuses_a_taken_name() {
    run(|c, h| async move {
        let mut state = drop_fixture(&c, &h).await;
        let dest = Disk::new();
        let source = Disk::new();
        // The host already holds a file under the second selection's name.
        fs::write(dest.0.join("taken.txt"), b"original host bytes").unwrap();
        let first = pseudo_random(120_007, 0x5eed);
        let first_name = "photo-ü λ.bin";
        fs::write(source.0.join(first_name), &first).unwrap();
        fs::write(source.0.join("taken.txt"), b"would overwrite").unwrap();
        let directory = Directory::open(&dest.0, Limits::default()).unwrap();
        let selection =
            Selection::select(&[source.0.join(first_name), source.0.join("taken.txt")]).unwrap();
        let mut host = HostLane::new(&directory);
        let control = SendControl::new();
        let mut viewer = ViewerLane::new(selection.request().unwrap(), control.clone());
        let view = state.host_media.binding();
        let mut tickets = 700_000_u128;
        let mut ticket = || nonce(&mut tickets);
        // No renewal yet: the host lane cannot offer, whatever it is asked.
        assert_eq!(state.host.control_renewed_until(), None);
        host.host(&mut state.host, view, &mut ticket);
        assert!(!state.host.file_receive_negotiating());
        assert_eq!(control.status().phase, SendPhase::WaitingForControl);
        // The viewer's first controlled turn installs the expectation.
        viewer.viewer(&mut state.viewer);
        assert!(state.viewer.file_send_negotiating());
        assert_eq!(control.status().phase, SendPhase::Negotiating);
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 3_000_000;
                let mut offered_after = None;
                loop {
                    assert!(now(&c).unwrap() < until, "{:?}", control.status());
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    let renewed = state.host.control_renewed_until();
                    host.host(&mut state.host, view, &mut ticket);
                    if offered_after.is_none()
                        && (state.host.file_receive_negotiating()
                            || state.host.file_receive_state().is_some())
                    {
                        offered_after = Some(renewed);
                    }
                    viewer.viewer(&mut state.viewer);
                    if control.status().phase == SendPhase::Finished {
                        break;
                    }
                }
                // The offer happened only once a renewal was on record.
                assert!(offered_after.is_some_and(|renewed| renewed.is_some()));
                let status = control.status();
                assert_eq!(status.absence, None);
                let report = status.report.unwrap();
                assert!(report.complete);
                assert_eq!((report.total, report.started), (2, 2));
                let receipts: Vec<_> = report.receipts().collect();
                assert_eq!(
                    receipts[0].outcome,
                    Outcome::HostPublished {
                        bytes: first.len() as u64,
                        publication: Publication::Durable,
                    }
                );
                assert_eq!(receipts[1].outcome, Outcome::HostRefused(Reason::Conflict));
                assert_eq!(
                    report.stop,
                    Some(fr_files::sender::batch::Stop::HostRefused(Reason::Conflict))
                );
                // Identical bytes under the exact selected basename; the taken
                // name keeps its original bytes; no staging file is left.
                assert_eq!(fs::read(dest.0.join(first_name)).unwrap(), first);
                assert_eq!(
                    fs::read(dest.0.join("taken.txt")).unwrap(),
                    b"original host bytes"
                );
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 2);
                // Control is untouched by the file lane.
                let _ = state.viewer.action(key(true)).unwrap();
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until + 1_000_000);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
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

#[test]
fn a_session_without_the_drop_selection_gets_a_typed_absence_and_no_lane() {
    run(|c, h| async move {
        // Files attachment WITHOUT channel scope: the drop lane is absent.
        let mut state = Box::pin(fixture_with_clipboard(
            &c,
            &h,
            caps(),
            false,
            false,
            false,
            ClipboardMode::FilesNegotiate,
        ))
        .await;
        assert_eq!(
            crate::session_startup::native_control::files_lane(
                &state.viewer.session.opened.selection
            ),
            Err(Absence::NotNegotiated)
        );
        // A lane forced onto it anyway refuses locally, spends no channel
        // and leaves control running.
        let source = Disk::new();
        fs::write(source.0.join("a.bin"), b"a").unwrap();
        let control = SendControl::new();
        let mut viewer = ViewerLane::new(
            Selection::select(&[source.0.join("a.bin")])
                .unwrap()
                .request()
                .unwrap(),
            control.clone(),
        );
        viewer.viewer(&mut state.viewer);
        let status = control.status();
        assert_eq!(status.phase, SendPhase::Ended);
        assert!(matches!(status.absence, Some(Absence::Refused(_))));
        assert!(!state.viewer.file_send_negotiating());
        assert!(!state.viewer.is_closed());
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
