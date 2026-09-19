//! Real ATP, TLS/UDP, disk and the existing control/renewal/native-agent owners.
//! Visibility/initial consent and the counted input sink remain explicit fixtures.
use super::*;
use crate::session_startup::{FileReceiveCleanup, FileReceiveError};
use fr_files::{
    quic::{Configuration, State},
    receive::{DropDirectory, Limits, Publication},
    sender::{Outcome, Sender, Stage},
    session::Permission,
};
use fr_transport::quic::files::FilesChannel;
use std::{
    fs::{self, File},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::AtomicUsize,
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-running-files-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn configuration(&self, permission: Permission) -> Configuration {
        Configuration {
            directory: DropDirectory::open(
                &self.0,
                Limits {
                    max_file_bytes: 2_000_000,
                    max_reserved_bytes: 4_000_000,
                    max_transfers: 2,
                },
            )
            .unwrap(),
            permission,
            policy: fr_files::session::Policy::conservative(),
            reply_lifetime: Duration::from_secs(1),
        }
    }
}
async fn attached(
    host: &mut ControlledHost,
    viewer: &mut View,
    c: &Cx,
    h: &Cx,
) -> (MediaChannel, FilesChannel) {
    let (hc, vc) = attach(
        &mut host.session,
        &mut viewer.session,
        c,
        h,
        MediaRole::Files,
        14,
    )
    .await;
    let client =
        FilesChannel::new(viewer.session.io().unwrap().0, vc, credentials().lease, 71).unwrap();
    (hc, client)
}
async fn step(
    host: &mut ControlledHost,
    viewer: &mut View,
    sender: &mut Sender<'_>,
    c: &Cx,
    n: &mut u128,
    t: &mut u128,
) {
    sender
        .service(viewer.session.io().unwrap().0, || true)
        .unwrap();
    let (result, ()) = Box::pin(support::both(
        host.drive(
            Duration::from_millis(2),
            || nonce(n),
            || ticket(t),
            |route, _| {
                assert!(!matches!(route, Route::Stream(r) if r.messages == Messages::Files));
                Ok(Disposition::Blocked)
            },
        ),
        viewer.drive(c),
    ))
    .await;
    result.unwrap();
    sender
        .service(viewer.session.io().unwrap().0, || true)
        .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)]
fn regular_controller_transfers_verified_files_while_input_and_renewal_continue() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            effects,
            seat,
            initial_until,
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let data: Vec<u8> = (0..120_007)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        let source = directory.0.join("selected-local-source");
        fs::write(&source, &data).unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let (channel, mut client) = attached(&mut host, &mut viewer, &c, &h).await;
                let connection = host.io().unwrap().0.binding();
                host.attach_files(channel, 71, directory.configuration(Permission::new(true)))
                    .unwrap();
                let mut sender = Sender::new(
                    c.clone(),
                    viewer.session.io().unwrap().0,
                    &mut client,
                    fr_files::sender::Policy::default(),
                )
                .unwrap();
                assert_eq!(
                    sender.begin(
                        viewer.session.io().unwrap().0,
                        File::open(&source).unwrap(),
                        "received.bin"
                    ),
                    Ok(1)
                );
                let (mut n, mut t) = (3000, 6000);
                let mut result = None;
                let mut pressed = false;
                let mut released = false;
                while result.is_none()
                    || now(&h).unwrap() < initial_until + 100_000
                    || viewer.receipts < 2
                {
                    assert!(now(&h).unwrap() < initial_until + 1_000_000);
                    viewer.visible(&c);
                    if !pressed && viewer.tickets > 0 {
                        viewer.queue_key(&c, true);
                        pressed = true;
                    }
                    if pressed
                        && !released
                        && viewer.receipts == 1
                        && now(&h).unwrap() > initial_until
                    {
                        viewer.queue_key(&c, false);
                        released = true;
                    }
                    step(&mut host, &mut viewer, &mut sender, &c, &mut n, &mut t).await;
                    result = result.or_else(|| sender.take_result());
                }
                assert_eq!(
                    result.unwrap().outcome,
                    Outcome::HostPublished {
                        bytes: 120_007,
                        publication: Publication::Durable
                    }
                );
                assert_eq!(fs::read(directory.0.join("received.bin")).unwrap(), data);
                assert_eq!(effects.lock().unwrap().events, [true, false]);
                assert!(host.control_renewed_until().unwrap().as_micros() > initial_until);
                assert!(host.observation_renewed_until().unwrap().as_micros() > initial_until);
                assert!(host.io().unwrap().0.is_bound_to(&connection));
                // A second object on the same original lane retains its ID ledger.
                let empty = directory.0.join("empty-source");
                fs::write(&empty, b"").unwrap();
                assert_eq!(
                    sender.begin(
                        viewer.session.io().unwrap().0,
                        File::open(&empty).unwrap(),
                        "empty"
                    ),
                    Ok(2)
                );
                let until = now(&c).unwrap() + 1_000_000;
                loop {
                    assert!(now(&c).unwrap() < until);
                    step(&mut host, &mut viewer, &mut sender, &c, &mut n, &mut t).await;
                    if let Some(result) = sender.take_result() {
                        assert_eq!(result.id, 2);
                        assert_eq!(
                            result.outcome,
                            Outcome::HostPublished {
                                bytes: 0,
                                publication: Publication::Durable
                            }
                        );
                        break;
                    }
                }
                assert_eq!(fs::read(directory.0.join("empty")).unwrap(), b"");
                let published = host.file_receive_result().unwrap();
                assert_eq!(published.id, 2);
                assert!(matches!(
                    published.outcome,
                    Ok(fr_files::worker::Completion::Published(_))
                ));
                assert_eq!(host.file_receive_reason(), None);
                // File retirement revokes the independent file permission. Preserve
                // the worker's Permission stop reason, not a fabricated clean exit.
                host.retire_files().unwrap();
                assert!(!host.control().is_stopped());
                assert!(host.io().unwrap().0.is_bound_to(&connection));
                assert_eq!(
                    host.reap_files(
                        &c,
                        crate::worker::Deadline::after(&c, Duration::from_secs(1)).unwrap()
                    )
                    .await,
                    Ok(FileReceiveCleanup::Finished(Err(
                        fr_files::worker::Error::Session(fr_files::session::Error::Permission,)
                    )))
                );
                assert_eq!(host.file_receive_result(), Some(published));
                sender.cancel(viewer.session.io().unwrap().0).unwrap();
                host.close();
                assert_eq!(
                    host.file_receive_cleanup(),
                    FileReceiveCleanup::Finished(Err(fr_files::worker::Error::Session(
                        fr_files::session::Error::Permission,
                    )))
                );
                assert_eq!(host.file_receive_result(), Some(published));
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn cancelling_partial_files_reclaims_staging_without_stopping_desktop_control() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            effects,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let source = directory.0.join("source");
        fs::write(&source, vec![7_u8; 500_000]).unwrap();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let (channel, mut client) = attached(&mut host, &mut viewer, &c, &h).await;
                host.attach_files(channel, 71, directory.configuration(Permission::new(true)))
                    .unwrap();
                let mut sender = Sender::new(
                    c.clone(),
                    viewer.session.io().unwrap().0,
                    &mut client,
                    fr_files::sender::Policy {
                        bytes_per_second: 65_536,
                        ..Default::default()
                    },
                )
                .unwrap();
                sender
                    .begin(
                        viewer.session.io().unwrap().0,
                        File::open(&source).unwrap(),
                        "unfinished",
                    )
                    .unwrap();
                let (mut n, mut t) = (3000, 6000);
                let until = now(&h).unwrap() + 2_000_000;
                while host
                    .file_receive_progress()
                    .is_none_or(|p| p.staged_bytes == 0)
                {
                    assert!(now(&h).unwrap() < until);
                    step(&mut host, &mut viewer, &mut sender, &c, &mut n, &mut t).await;
                }
                assert_eq!(sender.stage(), Stage::Streaming);
                // File retirement revokes the independent file permission. Preserve
                // the worker's Permission stop reason, not a fabricated clean exit.
                host.retire_files().unwrap();
                assert_eq!(host.file_receive_state(), Some(State::Retired));
                assert_eq!(
                    host.reap_files(
                        &c,
                        crate::worker::Deadline::after(&c, Duration::from_secs(1)).unwrap()
                    )
                    .await,
                    Ok(FileReceiveCleanup::Finished(Err(
                        fr_files::worker::Error::Session(fr_files::session::Error::Permission,)
                    )))
                );
                assert!(!directory.0.join("unfinished").exists());
                assert!(fs::read_dir(&directory.0).unwrap().all(|p| {
                    !p.unwrap()
                        .file_name()
                        .to_string_lossy()
                        .starts_with(".fr-part-")
                }));
                assert!(!host.control().is_stopped());
                sender.cancel(viewer.session.io().unwrap().0).unwrap();
                // Continue the same controller after file teardown, including actual
                // production agent receipts; no replacement authority is created.
                viewer.visible(&c);
                viewer.queue_key(&c, true);
                while viewer.receipts < 1 {
                    assert!(now(&h).unwrap() < until);
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                viewer.visible(&c);
                viewer.queue_key(&c, false);
                while viewer.receipts < 2 {
                    assert!(now(&h).unwrap() < until);
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert_eq!(effects.lock().unwrap().events, [true, false]);
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[test]
fn independent_file_permission_denial_does_not_allocate_a_disk_owner_or_revoke_input() {
    run(|c, h| async move {
        let Fixture {
            mut host,
            mut viewer,
            driver,
            seat,
            ..
        } = Box::pin(fixture_capabilities(&c, &h, None, false, true)).await;
        let directory = Directory::new();
        let ((), shutdown) = Box::pin(support::both(
            async {
                let (channel, mut client) = attached(&mut host, &mut viewer, &c, &h).await;
                assert!(matches!(
                    host.attach_files(channel, 71, directory.configuration(Permission::new(false))),
                    Err(FileReceiveError::Receiver(_))
                ));
                assert!(host.file_receive_reason().is_some());
                assert_eq!(host.file_receive_state(), None);
                assert_eq!(host.file_receive_cleanup(), FileReceiveCleanup::NotStarted);
                assert!(!host.control().is_stopped());
                let (mut n, mut t) = (3000, 6000);
                for _ in 0..10 {
                    turn(&mut host, &mut viewer, &c, &mut n, &mut t).await;
                }
                assert!(!host.control().is_stopped());
                client.retire(viewer.session.io().unwrap().0, &c).unwrap();
                host.close();
            },
            driver,
        ))
        .await;
        assert!(shutdown.handoff_safe());
        assert!(!seat.is_occupied());
    });
}

#[path = "files/negotiation.rs"]
mod negotiation;
