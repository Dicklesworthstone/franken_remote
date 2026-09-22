//! Actual running-controller file service with fixture consent and input sink;
//! file source/destination, ATP, encryption and UDP are real implementations.
use super::*;

fn source_tree(source: &Disk, bytes: &[u8]) -> File {
    fs::create_dir_all(source.0.join("src/deep")).unwrap();
    fs::write(source.0.join("README"), b"project").unwrap();
    fs::write(source.0.join("src/deep/data"), bytes).unwrap();
    fs::write(source.0.join("src/empty"), b"").unwrap();
    File::open(&source.0).unwrap()
}

#[test]
fn running_controller_publishes_directory_while_input_and_renewal_continue() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        let bytes = vec![0x61; 120_007];
        let selected = source_tree(&source, &bytes);
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let (mut n, mut t) = (80_000, 90_000);
                let started = now(&c).unwrap();
                let original_lease = state.viewer.input.binding().lease;
                assert_eq!(state.viewer.send_directory(selected, "project").unwrap(), 1);
                assert_eq!(
                    state
                        .viewer
                        .send_directory(File::open(&source.0).unwrap(), "busy"),
                    Err(SendError::Busy)
                );
                let _ = state.viewer.action(key(true)).unwrap();
                let mut released = false;
                let until = started + 2_000_000;
                loop {
                    assert!(now(&c).unwrap() < until, "directory transfer stalled");
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
                assert_eq!(
                    receipt.outcome,
                    Outcome::HostPublished {
                        bytes: bytes.len() as u64 + 7,
                        publication: Publication::Durable
                    }
                );
                assert_eq!(
                    fs::read(dest.0.join("project/src/deep/data")).unwrap(),
                    bytes
                );
                assert_eq!(fs::read(dest.0.join("project/README")).unwrap(), b"project");
                assert_eq!(
                    fs::metadata(dest.0.join("project/src/empty"))
                        .unwrap()
                        .len(),
                    0
                );
                assert!(!dest.0.join("busy").exists());
                // Continue beyond the provisional lease lifetime: successful bulk
                // publication must neither stop renewal nor replace the lease owner.
                while now(&c).unwrap() < started + 3_200_000 {
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert_eq!(state.effects.lock().unwrap().keys, [true, false]);
                assert_eq!(state.viewer.input.binding().lease, original_lease);
                assert!(
                    state.host.control_renewed_until().unwrap().as_micros() > started + 3_000_000
                );
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
fn revoked_file_permission_refuses_directory_without_touching_input_authority() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        let selected = source_tree(&source, b"private");
        let permission = Permission::new(true);
        join(&mut state, &dest, permission.clone());
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                permission.revoke();
                assert_eq!(
                    state.viewer.send_directory(selected, "denied"),
                    Err(SendError::Cancelled)
                );
                assert!(state.viewer.file_cleanup_finished());
                assert_eq!(state.viewer.file_result(), None);
                let (mut n, mut t) = (80_000, 90_000);
                let _ = state.viewer.action(key(true)).unwrap();
                let until = now(&c).unwrap() + 1_000_000;
                while state.viewer.pending_actions() != 0 {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
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
fn cancelling_running_directory_cleans_the_entire_tree_but_keeps_control_live() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        let selected = source_tree(&source, &vec![9; 1_000_007]);
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                state.viewer.send_directory(selected, "partial").unwrap();
                let (mut n, mut t) = (80_000, 90_000);
                let until = now(&c).unwrap() + 1_000_000;
                while state
                    .viewer
                    .file_progress()
                    .is_none_or(|p| p.queued_bytes == 0)
                {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                state.viewer.cancel_files().unwrap();
                assert_eq!(
                    state.viewer.file_result().unwrap().outcome,
                    Outcome::InterruptedBeforePublication(SendError::Cancelled)
                );
                let _ = state.viewer.action(key(true)).unwrap();
                loop {
                    assert!(now(&c).unwrap() < until);
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                    if state.viewer.file_cleanup_finished()
                        && state.viewer.pending_actions() == 0
                        && matches!(
                            state.host.file_receive_cleanup(),
                            crate::session_startup::FileReceiveCleanup::Finished(_)
                        )
                    {
                        break;
                    }
                }
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                assert!(!state.viewer.is_closed());
                assert!(state.viewer.take_file_result().is_some());
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
