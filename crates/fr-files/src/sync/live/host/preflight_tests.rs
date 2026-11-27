//! Real source workers, original job owners, UDP/TLS and filesystem publication.
//! Mutation happens after the scan has finished and before the first source starts.
//! No test hook changes a production outcome or manufactures a publication receipt.
#[allow(dead_code)]
#[path = "../../../../tests/native_support/mod.rs"]
mod native_support;

use super::*;
use crate::{
    quic::{Configuration, Receiver},
    receive::{ConflictPolicy, DropDirectory, Limits},
    sender::{Error as SendError, Policy as SendPolicy, Stage},
    sync::live::{ControllerJob, MAX_SESSION_BYTES, MAX_SESSION_TRANSFERS},
};
use fr_client::input::InputClient;
use fr_core::{input_submission::InputSession, time::HostInstant};
use native_support::{Link, clock, runtime};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::AtomicU64,
    time::{Duration, Instant},
};

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-folder-preflight-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }

    fn empty(&self) -> bool {
        fs::read_dir(&self.0).unwrap().next().is_none()
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

struct Fixture {
    link: Link,
    source: HostJob,
    sender: Sender<'static>,
    destination: ControllerJob,
    receiver: Receiver,
    input: InputSession,
    controller: InputClient,
    permission: Permission,
    upload: bool,
    cleaned: bool,
}

impl Fixture {
    // Keep the real negotiated source/receiver setup and direction swap together.
    #[allow(clippy::too_many_lines)]
    async fn new(
        cx: &Cx,
        source: &Path,
        destination: &Path,
        upload: bool,
        recursive: bool,
        round_lifetime_us: u64,
    ) -> Self {
        let mut link = Link::new(cx).await;
        let version = Some(if recursive {
            sync_job_state::RECURSIVE_VERSION
        } else {
            sync_job_state::VERSION
        });
        let (host_lane, client_lane, input, controller, authority) = if upload {
            link.upload_sync_version(cx, version, true).await
        } else {
            link.download_sync_version(cx, version).await
        };
        let host_authority = Authority::from_input(&input);
        let (source_authority, destination_authority) = if upload {
            (authority, host_authority)
        } else {
            (host_authority, authority)
        };
        let (source_connection, destination_connection, source_lane, destination_lane) = if upload {
            (&link.c, &mut link.h, client_lane, host_lane)
        } else {
            (&link.h, &mut link.c, host_lane, client_lane)
        };
        let mut config = HostConfig::open(source, 41)
            .unwrap()
            .with_scan_interval(Duration::from_millis(100))
            .unwrap();
        if recursive {
            config = config.with_recursive();
        }
        config.round_lifetime_us = round_lifetime_us;
        let permission = Permission::new(true);
        let sender = Sender::owning_authorized(
            cx.clone(),
            source_connection,
            source_lane,
            SendPolicy::default(),
            source_authority.clone(),
            permission.clone(),
        )
        .unwrap();
        let source = if upload {
            HostJob::for_controller(
                config,
                cx.clone(),
                source_authority,
                permission.clone(),
                source_connection,
            )
        } else {
            HostJob::new(
                config,
                cx.clone(),
                source_authority,
                permission.clone(),
                source_connection,
            )
        }
        .unwrap();
        let directory = DropDirectory::open(
            destination,
            Limits {
                max_file_bytes: 8 * 1024 * 1024,
                max_reserved_bytes: 8 * 1024 * 1024,
                max_transfers: 1,
            },
        )
        .unwrap()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
        let mut destination = if upload {
            ControllerJob::for_host(41, directory)
        } else {
            ControllerJob::new(41, directory)
        }
        .unwrap();
        if recursive {
            destination = destination.with_recursive();
        }
        let receiver = Receiver::spawn_with_authority(
            cx.clone(),
            destination_connection,
            destination_lane,
            destination_authority,
            Configuration {
                directory: destination.directory(),
                permission: Permission::new(true),
                policy: crate::session::Policy {
                    max_session_transfers: MAX_SESSION_TRANSFERS,
                    max_session_bytes: MAX_SESSION_BYTES,
                    ..crate::session::Policy::conservative()
                },
                reply_lifetime: Duration::from_secs(1),
            },
        )
        .unwrap();
        Self {
            link,
            source,
            sender,
            destination,
            receiver,
            input,
            controller,
            permission,
            upload,
            cleaned: false,
        }
    }

    fn source_turn(&mut self) -> Result<(), LiveError> {
        let connection = if self.upload {
            &mut self.link.c
        } else {
            &mut self.link.h
        };
        self.source.service(&mut self.sender, connection, || true)
    }

    fn destination_turn(&mut self) -> Result<(), LiveError> {
        let connection = if self.upload {
            &mut self.link.h
        } else {
            &mut self.link.c
        };
        self.destination
            .service(&mut self.receiver, connection, || true)
    }

    async fn until(&mut self, cx: &Cx, done: impl Fn(&Self) -> bool) {
        let until = clock(cx) + 2_000_000;
        while !done(self) {
            assert!(
                clock(cx) < until,
                "folder preparation stalled: source={:?}, destination={:?}, sender={:?}",
                self.source.status(),
                self.destination.status(),
                self.sender.stage()
            );
            self.source_turn().unwrap();
            self.destination_turn().unwrap();
            self.link.drive(cx).await;
        }
    }

    async fn settled(&mut self, cx: &Cx, round: u64) {
        let until = clock(cx) + 2_000_000;
        while self.source.status().rounds_settled < round
            || self.destination.status().rounds_settled < round
        {
            assert!(
                clock(cx) < until,
                "folder settlement stalled: source={:?}, destination={:?}, sender={:?}",
                self.source.status(),
                self.destination.status(),
                self.sender.stage()
            );
            // Receipt of settlement can precede the receiver's local send
            // drain. Preserve this exact round boundary while driving that
            // drain, so the next scan cannot race the caller's edits.
            if self.source.status().rounds_settled < round {
                self.source_turn().unwrap();
            }
            self.destination_turn().unwrap();
            self.link.drive(cx).await;
        }
        assert_eq!(self.source.status().rounds_settled, round);
        assert_eq!(self.destination.status().rounds_settled, round);
    }

    async fn source_failure(&mut self, cx: &Cx) -> LiveError {
        let until = clock(cx) + 2_000_000;
        loop {
            assert!(clock(cx) < until, "the original source did not fail");
            if let Err(error) = self.source_turn() {
                return error;
            }
            self.destination_turn().unwrap();
            self.link.drive(cx).await;
        }
    }

    fn original_input_live(&self, cx: &Cx) {
        assert!(
            self.input
                .monitor()
                .deadline(HostInstant::from_micros(clock(cx)))
                .is_ok()
        );
        assert!(self.controller.stopped().is_none());
        assert!(!self.link.h.is_closed() && !self.link.c.is_closed());
    }

    fn no_offer(&self) {
        let connection = if self.upload {
            &self.link.c
        } else {
            &self.link.h
        };
        assert_eq!(
            self.sender
                .sync_lane()
                .retained_send_records(connection)
                .unwrap(),
            0
        );
        assert_eq!(self.source.status().confirmed_publications, 0);
        assert_eq!(self.destination.status().confirmed_publications, 0);
        assert_eq!(self.destination.status().state.phase, SyncPhase::Scanning);
    }

    fn retire(&mut self) {
        if self.cleaned {
            return;
        }
        let (source, destination) = if self.upload {
            (&mut self.link.c, &mut self.link.h)
        } else {
            (&mut self.link.h, &mut self.link.c)
        };
        self.source.stop();
        self.sender.cancel(source).unwrap();
        self.destination.stop();
        self.receiver.retire(destination).unwrap();
        let until = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(
                Instant::now() < until,
                "original worker cleanup did not finish"
            );
            let source = self.source.try_finish_cleanup(&mut self.sender);
            let destination = self.receiver.try_finish_cleanup();
            if let (Some(source), Some(destination)) = (source, destination) {
                source.unwrap();
                destination.unwrap();
                self.receiver.collect_after_close().unwrap();
                self.destination
                    .collect_after_close(&self.receiver)
                    .unwrap();
                self.destination.cleaned(destination);
                self.cleaned = true;
                break;
            }
            std::thread::yield_now();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.retire();
        self.link.c.close();
        self.link.h.close();
    }
}

fn selected_path(root: &Path, recursive: bool) -> PathBuf {
    if recursive {
        let selected = root.join("selected");
        fs::DirBuilder::new().mode(0o700).create(&selected).unwrap();
        // Empty structure selects ATP profile 3 and must not be prepared twice.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(selected.join("empty"))
            .unwrap();
        selected.join("payload")
    } else {
        root.join("selected")
    }
}

#[test]
fn first_source_changes_rescan_before_offering_and_preserve_the_new_version_ledger() {
    for upload in [false, true] {
        for recursive in [false, true] {
            runtime().block_on(async {
                let cx = Cx::current().unwrap();
                let source = Directory::new();
                let destination = Directory::new();
                let selected = selected_path(&source.0, recursive);
                fs::write(&selected, b"old").unwrap();
                let mut f =
                    Fixture::new(&cx, &source.0, &destination.0, upload, recursive, 2_000_000)
                        .await;
                f.until(&cx, |f| matches!(f.source.step, Step::StartTransfer))
                    .await;
                let revision = f.source.status().state.manifest_revision;
                let deadline = f.source.round_until;
                fs::write(&selected, b"new selected version").unwrap();
                f.until(&cx, |f| matches!(f.source.step, Step::StartScan))
                    .await;
                f.no_offer();
                assert!(destination.empty());
                assert_eq!(f.source.status().state.manifest_revision, revision);
                assert_eq!(f.source.round_until, deadline);
                assert_eq!(f.source.scan_retry.attempts, 1);
                assert_eq!(f.source.budget.files, 1);
                assert_eq!(f.source.budget.bytes, 3);
                assert!(f.source.ledger.is_empty());
                assert!(f.source.cursor.after().is_none());
                assert_eq!(f.sender.stage(), Stage::Idle);
                assert!(f.sender.batch_report().is_none());
                assert!(f.sender.cleanup_finished());

                f.until(&cx, |f| matches!(f.source.step, Step::StartTransfer))
                    .await;
                assert_eq!(f.source.scan_retry.attempts, 2);
                assert_eq!(f.source.round_until, deadline);
                assert_eq!(f.source.status().state.manifest_revision, revision);
                f.settled(&cx, 1).await;
                let target = if recursive {
                    destination.0.join("selected/payload")
                } else {
                    destination.0.join("selected")
                };
                assert_eq!(fs::read(&target).unwrap(), b"new selected version");
                assert_eq!(f.receiver.last_result().unwrap().id, 2);
                assert_eq!(f.source.budget.files, 2);
                assert_eq!(f.source.budget.bytes, 3 + 20);
                assert_eq!(f.source.status().confirmed_publications, 1);
                if recursive {
                    assert!(destination.0.join("selected/empty").is_dir());
                }

                fs::write(&target, b"independent destination edit").unwrap();
                f.settled(&cx, 2).await;
                assert_eq!(fs::read(&target).unwrap(), b"independent destination edit");
                assert_eq!(f.source.status().confirmed_publications, 1);
                assert_eq!(f.destination.status().confirmed_publications, 1);
                f.original_input_live(&cx);
                f.retire();
            });
        }
    }
}

#[test]
fn repeated_preoffer_edits_spend_the_original_attempts_ids_and_byte_budget() {
    for upload in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let source = Directory::new();
            let destination = Directory::new();
            let selected = source.0.join("selected");
            fs::write(&selected, [0_u8; 3]).unwrap();
            let mut f =
                Fixture::new(&cx, &source.0, &destination.0, upload, false, 2_000_000).await;
            let mut bytes = 0;
            let mut deadline = None;
            for attempt in 1..=MAX_SCAN_ATTEMPTS {
                f.until(&cx, |f| matches!(f.source.step, Step::StartTransfer))
                    .await;
                deadline.get_or_insert(f.source.round_until);
                assert_eq!(deadline, Some(f.source.round_until));
                bytes += fs::metadata(&selected).unwrap().len();
                assert_eq!(f.source.budget.files, u32::from(attempt));
                assert_eq!(f.source.budget.bytes, bytes);
                fs::write(&selected, vec![attempt; usize::from(attempt) + 10]).unwrap();
                if attempt < MAX_SCAN_ATTEMPTS {
                    f.until(&cx, |f| matches!(f.source.step, Step::StartScan))
                        .await;
                    f.no_offer();
                    assert!(f.sender.batch_report().is_none());
                    assert!(f.sender.cleanup_finished());
                }
            }
            assert_eq!(
                f.source_failure(&cx).await,
                LiveError::Source(SyncError::SourceChanged)
            );
            assert_eq!(f.source.scan_retry.attempts, MAX_SCAN_ATTEMPTS);
            assert_eq!(f.source.budget.files, u32::from(MAX_SCAN_ATTEMPTS));
            assert_eq!(f.source.budget.bytes, bytes);
            assert!(destination.empty());
            assert!(f.source.ledger.is_empty());
            assert!(f.source.cursor.after().is_none());
            assert_eq!(f.source.status().confirmed_publications, 0);
            f.original_input_live(&cx);
            f.retire();
            assert!(f.source.status().cleanup_complete);
        });
    }
}

#[test]
fn a_held_first_source_cannot_run_ordinary_service_and_revocation_cannot_start_a_retry() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"selected").unwrap();
        let mut f = Fixture::new(&cx, &source.0, &destination.0, true, false, 2_000_000).await;
        f.until(&cx, |f| matches!(f.source.step, Step::StartTransfer))
            .await;
        f.source_turn().unwrap();
        assert!(matches!(f.source.step, Step::PreparingTransfer));
        let _ = f
            .sender
            .poll_sync_preflight(&f.link.c, &mut || true)
            .unwrap();
        assert_eq!(f.sender.stage(), Stage::Preparing);
        assert_eq!(
            f.sender.service(&mut f.link.c, || true),
            Err(SendError::Busy)
        );
        f.no_offer();

        f.permission.revoke();
        assert_eq!(f.source_turn(), Err(LiveError::Cancelled));
        assert_eq!(f.source.scan_retry.attempts, 1);
        assert_eq!(f.source.budget.files, 1);
        assert!(destination.empty());
        f.original_input_live(&cx);
        f.retire();
        assert!(f.source.status().cleanup_complete);
        assert_eq!(f.source.status().confirmed_publications, 0);
    });
}

#[test]
fn the_original_round_deadline_expires_while_the_prepared_source_is_held() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"selected").unwrap();
        let mut f = Fixture::new(&cx, &source.0, &destination.0, false, false, 1_000_000).await;
        f.until(&cx, |f| matches!(f.source.step, Step::StartTransfer))
            .await;
        let deadline = f.source.round_until;
        f.source_turn().unwrap();
        let _ = f
            .sender
            .poll_sync_preflight(&f.link.h, &mut || true)
            .unwrap();
        assert!(matches!(f.source.step, Step::PreparingTransfer));
        assert_eq!(f.sender.stage(), Stage::Preparing);
        asupersync::time::sleep(
            cx.now(),
            Duration::from_micros(deadline.saturating_sub(clock(&cx)) + 1),
        )
        .await;
        assert_eq!(f.source_turn(), Err(LiveError::Expired));
        assert_eq!(f.source.round_until, deadline);
        assert_eq!(f.source.scan_retry.attempts, 1);
        assert!(destination.empty());
        f.retire();
        assert!(f.source.status().cleanup_complete);
        assert_eq!(f.source.status().confirmed_publications, 0);
    });
}

#[test]
fn a_change_after_the_first_offer_is_terminal_and_never_rescanned() {
    for upload in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let source = Directory::new();
            let destination = Directory::new();
            let selected = source.0.join("selected");
            fs::write(&selected, b"original selected version").unwrap();
            let mut f =
                Fixture::new(&cx, &source.0, &destination.0, upload, false, 2_000_000).await;
            f.until(&cx, |f| f.sender.stage() == Stage::AwaitingAcceptance)
                .await;
            assert_eq!(f.source.status().state.phase, SyncPhase::Transferring);
            let deadline = f.source.round_until;
            fs::write(&selected, b"changed after offer").unwrap();
            let error = f.source_failure(&cx).await;
            assert!(matches!(
                error,
                LiveError::Send(SendError::SourceChanged)
                    | LiveError::Transfer(batch::Stop::Local(SendError::SourceChanged))
            ));
            assert_eq!(f.source.round_until, deadline);
            assert_eq!(f.source.scan_retry.attempts, 1);
            assert_eq!(f.source.budget.files, 1);
            assert_eq!(f.source.status().confirmed_publications, 0);
            assert!(!destination.0.join("selected").exists());
            f.original_input_live(&cx);
            f.retire();
            assert!(destination.empty());
        });
    }
}
