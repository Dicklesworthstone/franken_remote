//! Scope agreement via the actual completed attachment, never a remote path.
use super::*;
use crate::session_startup::FileReceiveError;

async fn scope_fixture(c: &Cx, h: &Cx) -> Fixture {
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
fn expect_drop(state: &mut Fixture, permission: Permission, timeout: Duration) {
    state
        .viewer
        .expect_file_drop(permission, fr_files::sender::Policy::default(), timeout)
        .unwrap();
    assert!(state.viewer.file_send_negotiating());
    assert_eq!(state.viewer.file_stage(), None);
    assert!(state.viewer.file_cleanup_finished());
}
#[test]
#[allow(clippy::too_many_lines)]
fn authenticated_channel_agrees_scope_and_publishes_successive_selected_files() {
    run(|c, h| async move {
        let mut state = scope_fixture(&c, &h).await;
        let dest = Disk::new();
        let source = Disk::new();
        // Neither API accepts a pre-agreed file handle. The host chooses a fresh
        // channel and maps it to its explicitly approved local drop directory.
        expect_drop(&mut state, Permission::new(true), Duration::from_secs(1));
        let req = request(&state, Duration::from_secs(1));
        assert_ne!(req.binding.parent.id, 77);
        state.host.offer_file_drop(req, dest.config()).unwrap();
        assert_eq!(state.host.file_receive_state(), None);
        assert_eq!(
            state.viewer.send_file(source.source(b"not yet"), "early"),
            Err(SendError::Busy)
        );
        let original_lease = state.viewer.input.binding().lease;
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 2_000_000;
                while state.viewer.file_send_negotiating() || state.host.file_receive_negotiating()
                {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert_eq!(state.viewer.file_stage(), Some(Stage::Idle));
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                for (id, name, bytes) in [(1, "large", vec![0x63; 120_007]), (2, "empty", vec![])] {
                    assert_eq!(
                        state.viewer.send_file(source.source(&bytes), name).unwrap(),
                        id
                    );
                    let _ = state.viewer.action(key(true)).unwrap();
                    let mut released = false;
                    loop {
                        assert!(now(&c).unwrap() < until);
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                        if !released && state.viewer.pending_actions() == 0 {
                            let _ = state.viewer.action(key(false)).unwrap();
                            released = true;
                        }
                        if state.viewer.file_result().is_some()
                            && state.viewer.file_cleanup_finished()
                            && released
                            && state.viewer.pending_actions() == 0
                        {
                            break;
                        }
                    }
                    let receipt = state.viewer.take_file_result().unwrap();
                    assert_eq!(receipt.id, id);
                    assert_eq!(
                        receipt.outcome,
                        Outcome::HostPublished {
                            bytes: bytes.len() as u64,
                            publication: Publication::Durable,
                        }
                    );
                    assert_eq!(fs::read(dest.0.join(name)).unwrap(), bytes);
                }
                assert_eq!(state.viewer.input.binding().lease, original_lease);
                assert_eq!(
                    state.effects.lock().unwrap().keys,
                    [true, false, true, false]
                );
                assert!(!dest.0.join("early").exists());
                state.viewer.cancel_files().unwrap();
                for _ in 0..10 {
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
        assert!(!state.seat.is_occupied());
    });
}
#[test]
fn absent_scope_negotiation_refuses_both_apis_without_spending_a_channel() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        let dest = Disk::new();
        let before_host = state.host.io().unwrap().0.next_channel_binding().unwrap();
        let before_viewer = state
            .viewer
            .session
            .transport
            .next_channel_binding()
            .unwrap();
        assert_eq!(
            state.viewer.expect_file_drop(
                Permission::new(true),
                fr_files::sender::Policy::default(),
                Duration::from_secs(1)
            ),
            Err(SendError::WrongRole)
        );
        assert_eq!(
            state
                .host
                .offer_file_drop(request(&state, Duration::from_secs(1)), dest.config()),
            Err(FileReceiveError::NotNegotiated)
        );
        assert_eq!(
            state.host.io().unwrap().0.next_channel_binding().unwrap(),
            before_host
        );
        assert_eq!(
            state
                .viewer
                .session
                .transport
                .next_channel_binding()
                .unwrap(),
            before_viewer
        );
        assert!(!state.viewer.file_send_negotiating() && !state.host.file_receive_negotiating());
        assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
        // Legacy explicitly agreed scopes remain available to older peers.
        expect(&mut state, Permission::new(true), Duration::from_secs(1));
        state
            .host
            .offer_files(request(&state, Duration::from_secs(1)), 77, dest.config())
            .unwrap();
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn channel_scope_never_substitutes_for_independent_local_permission() {
    run(|c, h| async move {
        let mut state = scope_fixture(&c, &h).await;
        let dest = Disk::new();
        let before = state.host.io().unwrap().0.next_channel_binding().unwrap();
        assert_eq!(
            state.viewer.expect_file_drop(
                Permission::new(false),
                fr_files::sender::Policy::default(),
                Duration::from_secs(1)
            ),
            Err(SendError::Cancelled)
        );
        let config = dest.config();
        config.permission.revoke();
        assert_eq!(
            state
                .host
                .offer_file_drop(request(&state, Duration::from_secs(1)), config),
            Err(FileReceiveError::PermissionRequired)
        );
        assert_eq!(
            state.host.io().unwrap().0.next_channel_binding().unwrap(),
            before
        );
        assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
        // Both refused calls leave the one-shot slots untouched.
        expect_drop(&mut state, Permission::new(true), Duration::from_secs(1));
        state
            .host
            .offer_file_drop(request(&state, Duration::from_secs(1)), dest.config())
            .unwrap();
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn channel_scope_expectation_keeps_its_original_prepoll_deadline() {
    run(|c, h| async move {
        let mut state = scope_fixture(&c, &h).await;
        expect_drop(&mut state, Permission::new(true), Duration::from_millis(30));
        asupersync::time::sleep(c.now(), Duration::from_millis(40)).await;
        assert_eq!(
            state.viewer.drive(Duration::ZERO, |_| {}, block).await,
            Err(Error::Files(SendError::Expired))
        );
        stopped_without_source(&state);
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn channel_scope_cannot_adopt_another_displays_authenticated_offer() {
    run(|c, h| async move {
        let mut state = scope_fixture(&c, &h).await;
        let dest = Disk::new();
        expect_drop(&mut state, Permission::new(true), Duration::from_secs(1));
        let mut wrong = request(&state, Duration::from_secs(1));
        wrong.binding.display += 1;
        state.host.offer_file_drop(wrong, dest.config()).unwrap();
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&h).unwrap() + 500_000;
                loop {
                    assert!(now(&h).unwrap() < until);
                    let (host, viewer) = turn(&mut state, &c, &h, &mut n, &mut t).await;
                    host.unwrap();
                    if let Err(error) = viewer {
                        assert_eq!(
                            error,
                            Error::Files(SendError::Transport(quic::Error::Handler))
                        );
                        break;
                    }
                }
                stopped_without_source(&state);
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert_eq!(state.host.file_receive_state(), None);
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}
