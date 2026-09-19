//! Real TLS/UDP/ATP/disk work through BOTH running controllers. Desktop pixels,
//! initial consent and the effect-counting input sink are explicit fixtures.
use super::*;
use fr_files::{
    receive::{DropDirectory, Publication},
    sender::{Error as SendError, Outcome, Stage},
    session::Permission,
};
use std::{
    fs::{self, File},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Disk(PathBuf);
impl Disk {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fr-viewer-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        Self(dir)
    }
    fn config(&self) -> fr_files::quic::Configuration {
        fr_files::quic::Configuration {
            directory: DropDirectory::open(
                &self.0,
                fr_files::receive::Limits {
                    max_file_bytes: 2_000_000,
                    max_reserved_bytes: 4_000_000,
                    max_transfers: 2,
                },
            )
            .unwrap(),
            permission: Permission::new(true),
            policy: fr_files::session::Policy::conservative(),
            reply_lifetime: Duration::from_secs(1),
        }
    }
    fn source(&self, bytes: &[u8]) -> File {
        let p = self.0.join("source");
        fs::write(&p, bytes).unwrap();
        File::open(p).unwrap()
    }
}
async fn setup(c: &Cx, h: &Cx) -> Fixture {
    Box::pin(fixture_with_clipboard(
        c,
        h,
        caps(),
        false,
        false,
        false,
        ClipboardMode::Files,
    ))
    .await
}
fn join(state: &mut Fixture, dest: &Disk, permission: Permission) {
    let (hc, vc) = state.file_channels.take().unwrap();
    state.host.attach_files(hc, 77, dest.config()).unwrap();
    state
        .viewer
        .attach_files(vc, 77, permission, fr_files::sender::Policy::default())
        .unwrap();
}
async fn drive(state: &mut Fixture, c: &Cx, h: &Cx, n: &mut u128, t: &mut u128) {
    let (host, viewer) = turn(state, c, h, n, t).await;
    host.unwrap();
    viewer.unwrap();
}
#[test]
fn running_viewer_sends_successive_files_and_renews_input_on_the_same_connection() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                let mut n = 80000;
                let mut t = 90000;
                let started = now(&c).unwrap();
                let original_lease = state.viewer.input.binding().lease;
                for (id, name, bytes) in [(1, "large", vec![0x71; 120_007]), (2, "empty", vec![])] {
                    assert_eq!(
                        state.viewer.send_file(source.source(&bytes), name).unwrap(),
                        id
                    );
                    assert_eq!(
                        state
                            .viewer
                            .send_file(File::open(source.0.join("source")).unwrap(), "busy"),
                        Err(SendError::Busy)
                    );
                    let mut pressed = false;
                    let mut released = false;
                    let until = now(&c).unwrap() + 2_000_000;
                    loop {
                        assert!(now(&c).unwrap() < until, "bounded file completion");
                        drive(&mut state, &c, &h, &mut n, &mut t).await;
                        if !pressed {
                            let _ = state.viewer.action(key(true)).unwrap();
                            pressed = true;
                        } else if !released && state.viewer.pending_actions() == 0 {
                            let _ = state.viewer.action(key(false)).unwrap();
                            released = true;
                        }
                        if let Some(receipt) = state.viewer.file_result() {
                            assert_eq!(receipt.id, id);
                            assert_eq!(
                                receipt.outcome,
                                Outcome::HostPublished {
                                    bytes: bytes.len() as u64,
                                    publication: Publication::Durable
                                }
                            );
                            if state.viewer.file_cleanup_finished()
                                && released
                                && state.viewer.pending_actions() == 0
                            {
                                break;
                            }
                        }
                    }
                    assert_eq!(fs::read(dest.0.join(name)).unwrap(), bytes);
                    assert_eq!(state.viewer.take_file_result().unwrap().id, id);
                    assert!(state.viewer.file_result().is_none());
                    assert_eq!(state.viewer.file_stage(), Some(Stage::Idle));
                }
                while now(&c).unwrap() < started + 3_200_000 {
                    drive(&mut state, &c, &h, &mut n, &mut t).await;
                }
                assert_eq!(
                    state.effects.lock().unwrap().keys,
                    [true, false, true, false]
                );
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
fn cancelling_partial_file_preserves_control_and_reports_no_publication_request() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        join(&mut state, &dest, Permission::new(true));
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                state
                    .viewer
                    .send_file(source.source(&vec![9; 1_000_007]), "partial")
                    .unwrap();
                let mut n = 80000;
                let mut t = 90000;
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
                assert!(matches!(
                    state.viewer.file_result().unwrap().outcome,
                    Outcome::InterruptedBeforePublication(SendError::Cancelled)
                ));
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
                assert!(!dest.0.join("partial").exists());
                assert_eq!(fs::read_dir(&dest.0).unwrap().count(), 0);
                assert_eq!(state.effects.lock().unwrap().keys, [true]);
                assert!(state.viewer.take_file_result().is_some());
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
#[test]
fn revoked_local_file_permission_prevents_sending_without_revoking_input() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        let permission = Permission::new(true);
        join(&mut state, &dest, permission.clone());
        let driver = state.driver.take().unwrap();
        let ((), done) = Box::pin(support::both(
            async {
                permission.revoke();
                assert_eq!(
                    state
                        .viewer
                        .send_file(source.source(b"not authorized"), "denied"),
                    Err(SendError::Cancelled)
                );
                assert_eq!(state.viewer.file_stage(), Some(Stage::Closed));
                assert!(state.viewer.file_cleanup_finished());
                assert!(state.viewer.file_result().is_none());
                let mut n = 80000;
                let mut t = 90000;
                let _ = state.viewer.action(key(true)).unwrap();
                while state.viewer.pending_actions() != 0 {
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
    });
}
#[test]
fn closing_unpolled_turn_keeps_original_file_result_and_cleanup_accessible() {
    run(|c, h| async move {
        let mut state = setup(&c, &h).await;
        let source = Disk::new();
        let dest = Disk::new();
        join(&mut state, &dest, Permission::new(true));
        state
            .viewer
            .send_file(source.source(b"source"), "unpolled")
            .unwrap();
        drop(state.viewer.drive(Duration::ZERO, |_| {}, block));
        assert!(state.viewer.is_closed());
        assert_eq!(
            state.viewer.file_result().unwrap().outcome,
            Outcome::InterruptedBeforePublication(SendError::Cancelled)
        );
        let independent = h.clone();
        state
            .viewer
            .reap_files(
                &independent,
                crate::worker::Deadline::after(&independent, Duration::from_secs(1)).unwrap(),
            )
            .await
            .unwrap();
        assert!(state.viewer.take_file_result().is_some());
        assert!(!dest.0.join("unpolled").exists());
        state.host.close();
        assert!(state.driver.take().unwrap().await.handoff_safe());
    });
}
