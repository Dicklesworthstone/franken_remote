//! Actual optional-channel negotiation through both production controller loops.
//! No manual attachment pump and no source worker before the completed exchange.
use super::*;
use fr_transport::quic::ChannelRequest;
use fr_wire::{attachment::Ticket, decoder::Binding, negotiation::ControlBinding};

async fn pending_fixture(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        ClipboardMode::FilesNegotiate,
    ))
    .await
}
fn request(state: &Fixture, timeout: Duration) -> ChannelRequest {
    ChannelRequest {
        binding: Binding {
            parent: ControlBinding {
                id: 14,
                ..state.viewer.media.binding().parent
            },
            ..state.viewer.media.binding()
        },
        ticket: Ticket(814),
        timeout,
    }
}
fn expect(state: &mut Fixture, permission: Permission, timeout: Duration) {
    state
        .viewer
        .expect_files(77, permission, fr_files::sender::Policy::default(), timeout)
        .unwrap();
    assert!(state.viewer.file_send_negotiating());
    assert_eq!(state.viewer.file_stage(), None);
    assert!(state.viewer.file_cleanup_finished());
}
fn stopped_without_source(state: &Fixture) {
    assert!(state.viewer.is_closed());
    assert!(!state.viewer.file_send_negotiating());
    assert_eq!(state.viewer.file_stage(), None);
    assert_eq!(state.viewer.file_progress(), None);
    assert_eq!(state.viewer.file_result(), None);
    assert!(state.viewer.file_cleanup_finished());
}
#[test]
fn normal_controller_turns_negotiate_then_publish_selected_files_while_input_runs() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        let dest = Disk::new();
        let source = Disk::new();
        let bytes = vec![0x47; 120_007];
        expect(&mut state, Permission::new(true), Duration::from_secs(1));
        assert_eq!(
            state.viewer.send_file(source.source(&bytes), "too-early"),
            Err(SendError::Busy)
        );
        assert_eq!(
            state.viewer.expect_files(
                77,
                Permission::new(true),
                fr_files::sender::Policy::default(),
                Duration::from_secs(1)
            ),
            Err(SendError::Busy)
        );
        state
            .host
            .offer_files(request(&state, Duration::from_secs(1)), 77, dest.config())
            .unwrap();
        assert!(state.host.file_receive_negotiating());
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
                assert_eq!(
                    state
                        .viewer
                        .send_file(File::open(source.0.join("source")).unwrap(), "received")
                        .unwrap(),
                    1
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
                assert_eq!(
                    state.viewer.file_result().unwrap().outcome,
                    Outcome::HostPublished {
                        bytes: bytes.len() as u64,
                        publication: Publication::Durable
                    }
                );
                assert_eq!(fs::read(dest.0.join("received")).unwrap(), bytes);
                assert!(!dest.0.join("too-early").exists());
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                let receipt = state.viewer.file_result();
                state.viewer.cancel_files().unwrap();
                for _ in 0..10 {
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert!(!state.viewer.is_closed() && !state.host.control().is_stopped());
                assert_eq!(state.viewer.file_result(), receipt);
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
fn local_setup_refusals_do_not_spend_the_single_use_expectation() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        let before = state
            .viewer
            .session
            .transport
            .next_channel_binding()
            .unwrap();
        for (handle, approved, timeout, error) in [
            (77, false, Duration::from_secs(1), SendError::Cancelled),
            (
                0,
                true,
                Duration::from_secs(1),
                SendError::Wire(WireError::InvalidBinding),
            ),
            (77, true, Duration::ZERO, SendError::Limits),
            (77, true, Duration::from_secs(3), SendError::Limits),
        ] {
            assert_eq!(
                state.viewer.expect_files(
                    handle,
                    Permission::new(approved),
                    fr_files::sender::Policy::default(),
                    timeout
                ),
                Err(error)
            );
            assert!(!state.viewer.file_send_negotiating() && !state.viewer.is_closed());
            assert_eq!(state.viewer.file_stage(), None);
            assert_eq!(
                state
                    .viewer
                    .session
                    .transport
                    .next_channel_binding()
                    .unwrap(),
                before
            );
        }
        expect(&mut state, Permission::new(true), Duration::from_secs(1));
        // The two optional channels cannot compete to consume the same binding
        // handshake. Completing one leaves the other independently available.
        assert_eq!(
            state.viewer.expect_clipboard(Duration::from_secs(1), true),
            Err(crate::clipboard_quic::Error::AlreadyAttached)
        );
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn absent_positive_file_capabilities_refuse_before_expectation_or_worker() {
    run(|c, h| async move {
        let mut state = Box::pin(fixture(&c, &h)).await;
        assert_eq!(
            state.viewer.expect_files(
                77,
                Permission::new(true),
                fr_files::sender::Policy::default(),
                Duration::from_secs(1)
            ),
            Err(SendError::WrongRole)
        );
        assert!(!state.viewer.is_closed() && !state.viewer.file_send_negotiating());
        assert_eq!(state.viewer.file_stage(), None);
        state.viewer.close();
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn expectation_timeout_includes_idle_before_the_first_controller_turn() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        expect(&mut state, Permission::new(true), Duration::from_millis(30));
        asupersync::time::sleep(c.now(), Duration::from_millis(40)).await;
        assert_eq!(
            state.viewer.drive(Duration::ZERO, |_| {}, block).await,
            Err(Error::Files(SendError::Expired))
        );
        stopped_without_source(&state);
        assert_eq!(state.viewer.file_failure(), Some(SendError::Expired));
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn withheld_file_offer_expires_even_while_input_and_observation_are_serviced() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        let start = now(&c).unwrap();
        expect(
            &mut state,
            Permission::new(true),
            Duration::from_millis(100),
        );
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                loop {
                    assert!(
                        now(&h).unwrap() < start + 400_000,
                        "file expectation refreshed its deadline"
                    );
                    let (host, viewer) = turn(&mut state, &c, &h, &mut n, &mut t).await;
                    host.unwrap();
                    if viewer.is_err() {
                        break;
                    }
                }
                stopped_without_source(&state);
                assert_eq!(state.viewer.file_failure(), Some(SendError::Expired));
                assert!(c.timer_driver().unwrap().now().as_nanos() / 1000 >= start + 100_000);
                state.host.close();
            },
            driver,
        ))
        .await;
        assert!(done.handoff_safe());
    });
}
#[test]
fn cancelling_unfinished_expectation_fences_parent_without_source_or_receipt() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        expect(&mut state, Permission::new(true), Duration::from_secs(1));
        assert_eq!(state.viewer.cancel_files(), Err(SendError::Cancelled));
        stopped_without_source(&state);
        assert_eq!(state.viewer.file_failure(), Some(SendError::Cancelled));
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn revoking_local_permission_during_expectation_fences_before_source_start() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        let permission = Permission::new(true);
        expect(&mut state, permission.clone(), Duration::from_secs(1));
        permission.revoke();
        assert_eq!(
            state.viewer.drive(Duration::ZERO, |_| {}, block).await,
            Err(Error::Files(SendError::Cancelled))
        );
        stopped_without_source(&state);
        assert_eq!(state.viewer.file_failure(), Some(SendError::Cancelled));
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn abandoning_unpolled_negotiation_turn_fences_the_original_controller() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        expect(&mut state, Permission::new(true), Duration::from_secs(1));
        drop(state.viewer.drive(Duration::from_millis(1), |_| {}, block));
        stopped_without_source(&state);
        assert_eq!(state.viewer.file_failure(), Some(SendError::Cancelled));
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
#[test]
fn host_offered_other_display_is_not_adopted_as_this_viewers_file_scope() {
    run(|c, h| async move {
        let mut state = pending_fixture(&c, &h).await;
        let dest = Disk::new();
        expect(&mut state, Permission::new(true), Duration::from_secs(1));
        let mut wrong = request(&state, Duration::from_secs(1));
        wrong.binding.display += 1;
        state.host.offer_files(wrong, 77, dest.config()).unwrap();
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
                assert_eq!(
                    state.viewer.file_failure(),
                    Some(SendError::Transport(quic::Error::Handler))
                );
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

#[path = "negotiation/scope.rs"]
mod scope;
