#![cfg(target_os = "linux")]
//! Real UDP/TLS, ATP source workers and independently pinned destination disk.
//! The original control grant/presentation is an explicit fixture; no paths or
//! file content are injected through an alternate local-copy implementation.
#[allow(dead_code)]
mod native_support;
use asupersync::cx::Cx;
use fr_client::input::InputClient;
use fr_core::{input_submission::InputSession, time::HostInstant};
use fr_files::{
    quic::{Configuration, Receiver},
    receive::{ConflictPolicy, DropDirectory, Limits},
    sender::{Policy as SendPolicy, Sender, Stage},
    session::{Authority, Permission, Policy},
    sync::{ControllerJob, HostConfig, HostJob, LiveError, ReceiverJob, SourceJob},
};
use fr_wire::sync_job_state::SyncPhase;
use native_support::{Link, clock, runtime};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-remote-folder-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn files(&self) -> BTreeMap<String, Vec<u8>> {
        fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().into_string().unwrap(),
                    fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

fn controller(
    cx: &Cx,
    path: &Path,
    accepted_id: u64,
    client_connection: &mut fr_transport::quic::QuicRecords,
    client_lane: fr_transport::quic::files::FilesChannel,
    authority: Authority,
) -> (ControllerJob, Receiver) {
    receiver_owner(
        cx,
        path,
        accepted_id,
        client_connection,
        client_lane,
        authority,
        false,
    )
}

fn receiver_owner(
    cx: &Cx,
    path: &Path,
    job_id: u64,
    connection: &mut fr_transport::quic::QuicRecords,
    lane: fr_transport::quic::files::FilesChannel,
    authority: Authority,
    upload: bool,
) -> (ReceiverJob, Receiver) {
    receiver_owner_mode(
        cx,
        path,
        (job_id, false),
        connection,
        lane,
        authority,
        upload,
    )
}

fn receiver_owner_mode(
    cx: &Cx,
    path: &Path,
    job: (u64, bool),
    connection: &mut fr_transport::quic::QuicRecords,
    lane: fr_transport::quic::files::FilesChannel,
    authority: Authority,
    upload: bool,
) -> (ReceiverJob, Receiver) {
    let (job_id, recursive) = job;
    let directory = DropDirectory::open(
        path,
        Limits {
            max_file_bytes: 8 * 1024 * 1024,
            max_reserved_bytes: 8 * 1024 * 1024,
            max_transfers: 1,
        },
    )
    .unwrap()
    .with_conflict_policy(ConflictPolicy::KeepBoth);
    let client = if upload {
        ReceiverJob::for_host(job_id, directory)
    } else {
        ReceiverJob::new(job_id, directory)
    }
    .unwrap();
    let client = if recursive {
        client.with_recursive()
    } else {
        client
    };
    let receiver = Receiver::spawn_with_authority(
        cx.clone(),
        connection,
        lane,
        authority,
        Configuration {
            directory: client.directory(),
            permission: Permission::new(true),
            policy: Policy {
                max_session_transfers: fr_files::sync::live::MAX_SESSION_TRANSFERS,
                max_session_bytes: fr_files::sync::live::MAX_SESSION_BYTES,
                ..Policy::conservative()
            },
            reply_lifetime: Duration::from_secs(1),
        },
    )
    .unwrap();
    (client, receiver)
}

struct Running {
    link: Link,
    host: HostJob,
    sender: Sender<'static>,
    client: ControllerJob,
    receiver: Receiver,
    input: InputSession,
    source_permission: Permission,
    // The receiver's opaque authority must retain THIS accepted owner.
    _client_input: InputClient,
    cleaned: bool,
    upload: bool,
}
impl Running {
    async fn new(cx: &Cx, source: &Path, destination: &Path, accepted_id: u64) -> Self {
        Self::configured(cx, source, destination, accepted_id, false, false, true).await
    }

    async fn upload(
        cx: &Cx,
        source: &Path,
        destination: &Path,
        accepted_id: u64,
        duplex: bool,
        negotiated: bool,
    ) -> Self {
        Self::configured(
            cx,
            source,
            destination,
            accepted_id,
            true,
            duplex,
            negotiated,
        )
        .await
    }

    async fn configured(
        cx: &Cx,
        source: &Path,
        destination: &Path,
        accepted_id: u64,
        upload: bool,
        duplex: bool,
        negotiated: bool,
    ) -> Self {
        Self::configured_mode(
            cx,
            source,
            destination,
            (accepted_id, false),
            upload,
            duplex,
            negotiated,
        )
        .await
    }

    async fn configured_mode(
        cx: &Cx,
        source: &Path,
        destination: &Path,
        job: (u64, bool),
        upload: bool,
        duplex: bool,
        negotiated: bool,
    ) -> Self {
        let (accepted_id, recursive) = job;
        let mut link = Link::new(cx).await;
        let version = negotiated.then_some(if recursive {
            fr_wire::sync_job_state::RECURSIVE_VERSION
        } else {
            fr_wire::sync_job_state::VERSION
        });
        let (host_lane, client_lane, input, client_input, authority) = if upload {
            link.upload_sync_version(cx, version, duplex).await
        } else {
            link.download_sync_version(cx, version).await
        };
        let source = HostConfig::open(source, if upload { accepted_id } else { 41 })
            .unwrap()
            .with_scan_interval(Duration::from_millis(100))
            .unwrap();
        let source = if recursive {
            source.with_recursive()
        } else {
            source
        };
        let input_authority = Authority::from_input(&input);
        let (source_authority, receiver_authority) = if upload {
            (authority, input_authority)
        } else {
            (input_authority, authority)
        };
        let (source_connection, destination_connection, source_lane, destination_lane) = if upload {
            (&link.c, &mut link.h, client_lane, host_lane)
        } else {
            (&link.h, &mut link.c, host_lane, client_lane)
        };
        // The existing fixture field names retain their source/receiver roles;
        // an upload moves that source to the controller without swapping who
        // is permitted to advertise and explicitly accept the original job.
        let source_permission = Permission::new(true);
        let host = if upload {
            SourceJob::for_controller(
                source,
                cx.clone(),
                source_authority,
                source_permission.clone(),
                source_connection,
            )
        } else {
            SourceJob::new(
                source,
                cx.clone(),
                source_authority,
                source_permission.clone(),
                source_connection,
            )
        }
        .unwrap();
        let sender = Sender::owning(
            cx.clone(),
            source_connection,
            source_lane,
            SendPolicy::default(),
        )
        .unwrap();
        let (client, receiver) = receiver_owner_mode(
            cx,
            destination,
            (if upload { 41 } else { accepted_id }, recursive),
            destination_connection,
            destination_lane,
            receiver_authority,
            upload,
        );
        Self {
            link,
            host,
            sender,
            client,
            receiver,
            input,
            source_permission,
            _client_input: client_input,
            cleaned: false,
            upload,
        }
    }
    async fn step(&mut self, cx: &Cx) -> (Result<(), LiveError>, Result<(), LiveError>) {
        let (source, destination) = if self.upload {
            (&mut self.link.c, &mut self.link.h)
        } else {
            (&mut self.link.h, &mut self.link.c)
        };
        let host = self.host.service(&mut self.sender, source, || true);
        let client = self
            .client
            .service(&mut self.receiver, destination, || true);
        self.link.drive(cx).await;
        (host, client)
    }
    async fn settled(&mut self, cx: &Cx, round: u64) {
        let until = clock(cx) + 2_000_000;
        while self.host.status().rounds_settled < round
            || self.client.status().rounds_settled < round
        {
            assert!(
                clock(cx) < until,
                "standing round stalled: host={:?}, client={:?}, sender={:?}",
                self.host.status(),
                self.client.status(),
                self.sender.stage()
            );
            // The source sees settlement before the receiver necessarily sees
            // its QUIC acknowledgment drain. Keep the exact page boundary while
            // driving that drain; an immediate successor scan must not race the
            // caller's source edits or cancellation between settled pages.
            let (source, destination) = if self.upload {
                (&mut self.link.c, &mut self.link.h)
            } else {
                (&mut self.link.h, &mut self.link.c)
            };
            if self.host.status().rounds_settled < round {
                let host = self.host.service(&mut self.sender, source, || true);
                assert_eq!(host, Ok(()), "host status: {:?}", self.host.status());
            }
            let client = self
                .client
                .service(&mut self.receiver, destination, || true);
            assert_eq!(client, Ok(()), "client status: {:?}", self.client.status());
            self.link.drive(cx).await;
        }
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
        self.host.stop();
        self.sender.cancel(source).unwrap();
        self.client.stop();
        self.receiver.retire(destination).unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(
                Instant::now() < until,
                "original source/disk cleanup did not finish"
            );
            let host = self.host.try_finish_cleanup(&mut self.sender);
            let client = self.receiver.try_finish_cleanup();
            if let (Some(host), Some(client)) = (host, client) {
                host.unwrap();
                client.unwrap();
                self.receiver.collect_after_close().unwrap();
                self.client.collect_after_close(&self.receiver).unwrap();
                self.client.cleaned(client);
                self.cleaned = true;
                break;
            }
            std::thread::yield_now();
        }
    }
    fn input_live(&self, cx: &Cx) {
        assert!(
            self.input
                .monitor()
                .deadline(HostInstant::from_micros(clock(cx)))
                .is_ok()
        );
        assert!(!self.link.h.is_closed() && !self.link.c.is_closed());
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.retire();
        self.link.c.close();
        self.link.h.close();
    }
}

#[test]
fn standing_job_transfers_real_versions_keeps_conflicts_and_respects_independent_client_edits() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        let large: Vec<u8> = (0..120_007)
            .map(|n| u8::try_from(n % 251).unwrap())
            .collect();
        fs::write(source.0.join("large.bin"), &large).unwrap();
        fs::write(source.0.join("empty"), []).unwrap();
        fs::write(source.0.join("report.txt"), b"host version one").unwrap();
        fs::write(
            destination.0.join("report.txt"),
            b"original client document",
        )
        .unwrap();
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        running.settled(&cx, 1).await;
        assert_eq!(
            running.host.status().state.phase,
            SyncPhase::ConflictRecorded
        );
        assert_eq!(running.host.status().state.conflicts_kept, 1);
        assert_eq!(running.host.status().confirmed_publications, 3);
        assert_eq!(running.client.status().confirmed_publications, 3);
        assert_eq!(fs::read(destination.0.join("large.bin")).unwrap(), large);
        assert_eq!(
            fs::read(destination.0.join("empty")).unwrap(),
            [] as [u8; 0]
        );
        assert_eq!(
            fs::read(destination.0.join("report.txt")).unwrap(),
            b"original client document"
        );
        assert!(
            destination
                .files()
                .values()
                .any(|bytes| bytes == b"host version one")
        );

        // Local edits and deletion are not a request to replay an acknowledged
        // host version. Only newly selected SOURCE versions enter another ATP batch.
        fs::write(destination.0.join("large.bin"), b"independent client edit").unwrap();
        fs::remove_file(destination.0.join("empty")).unwrap();
        fs::write(source.0.join("report.txt"), b"host version two").unwrap();
        fs::write(source.0.join("added.txt"), b"new host file").unwrap();
        running.settled(&cx, 2).await;
        let second = running.host.status();
        assert_eq!(second.state.entries_total, 2);
        assert_eq!(second.state.entries_completed, 2);
        assert_eq!(second.state.conflicts_kept, 1);
        assert_eq!(second.confirmed_conflicts, 2);
        assert_eq!(second.confirmed_publications, 5);
        assert_eq!(
            fs::read(destination.0.join("large.bin")).unwrap(),
            b"independent client edit"
        );
        assert!(!destination.0.join("empty").exists());
        assert_eq!(
            fs::read(destination.0.join("added.txt")).unwrap(),
            b"new host file"
        );
        let second_files = destination.files();
        assert!(
            second_files
                .values()
                .any(|bytes| bytes == b"host version one")
        );
        assert!(
            second_files
                .values()
                .any(|bytes| bytes == b"host version two")
        );

        running.settled(&cx, 3).await;
        let third = running.host.status();
        assert_eq!(third.state.phase, SyncPhase::Settled);
        assert_eq!((third.state.entries_total, third.state.bytes_total), (0, 0));
        assert_eq!(third.confirmed_publications, 5);
        assert_eq!(third.confirmed_conflicts, 2);
        assert_eq!(running.client.status().confirmed_conflicts, 2);
        assert_eq!(running.client.status().confirmed_publications, 5);
        assert_eq!(destination.files(), second_files);
        running.input_live(&cx);
        running.retire();
        assert!(running.host.status().cleanup_complete && running.client.status().cleanup_complete);
        assert_eq!(running.host.status().confirmed_publications, 5);
        running.link.control_progress(&cx).await;
    });
}

#[test]
fn job_id_acceptance_precedes_even_source_scanning_or_any_file_offer() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        // This entry would refuse the bounded flat-folder scanner if it ran.
        fs::create_dir(source.0.join("must-not-be-scanned")).unwrap();
        let mut running = Running::new(&cx, &source.0, &destination.0, 99).await;
        for _ in 0..8 {
            running
                .host
                .service(&mut running.sender, &mut running.link.h, || true)
                .unwrap();
            running.link.drive(&cx).await;
        }
        assert_eq!(running.sender.stage(), Stage::Idle);
        assert_eq!(running.host.status().state.manifest_revision, 0);
        assert_eq!(running.host.status().failure, None);
        assert!(running.receiver.last_result().is_none());
        assert!(destination.files().is_empty());
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until);
            let (host, client) = running.step(&cx).await;
            host.unwrap();
            if let Err(error) = client {
                assert_eq!(error, LiveError::WrongJob);
                break;
            }
        }
        assert_eq!(running.host.status().state.manifest_revision, 0);
        assert_eq!(running.client.status().confirmed_publications, 0);
        assert!(destination.files().is_empty());
        running.input_live(&cx);
        running.retire();
        running.link.control_progress(&cx).await;
    });
}

#[test]
fn a_replacement_connection_cannot_resume_or_damage_the_original_job() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"original source").unwrap();
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        let mut replacement = Link::new(&cx).await;
        assert_eq!(
            running
                .host
                .service(&mut running.sender, &mut replacement.h, || true),
            Err(LiveError::WrongConnection)
        );
        assert!(matches!(
            running
                .client
                .service(&mut running.receiver, &mut replacement.c, || true),
            Err(LiveError::WrongConnection
                | LiveError::Receive(fr_files::quic::Error::WrongConnection))
        ));
        assert_eq!(running.host.status().failure, None);
        assert_eq!(running.client.status().failure, None);
        assert!(!replacement.h.is_closed() && !replacement.c.is_closed());
        running.settled(&cx, 1).await;
        assert_eq!(
            fs::read(destination.0.join("selected")).unwrap(),
            b"original source"
        );
        assert_eq!(running.host.status().confirmed_publications, 1);
        replacement.h.close();
        replacement.c.close();
    });
}

#[test]
fn cancel_during_second_file_preserves_confirmed_prefix_and_cleans_private_staging() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("a-confirmed"), b"confirmed first").unwrap();
        fs::write(source.0.join("z-partial"), vec![0x53; 4 * 1024 * 1024]).unwrap();
        fs::write(destination.0.join("a-confirmed"), b"client original").unwrap();
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        let until = clock(&cx) + 2_000_000;
        while running
            .receiver
            .progress()
            .is_none_or(|progress| progress.id != 2 || progress.staged_bytes == 0)
        {
            assert!(clock(&cx) < until, "second file did not stage");
            let (host, client) = running.step(&cx).await;
            host.unwrap();
            client.unwrap();
        }
        assert_eq!(running.host.status().confirmed_publications, 1);
        assert_eq!(
            fs::read(destination.0.join("a-confirmed")).unwrap(),
            b"client original"
        );
        assert!(!destination.0.join("z-partial").exists());
        running.retire();
        assert_eq!(running.host.status().failure, Some(LiveError::Cancelled));
        assert_eq!(running.host.status().rounds_settled, 0);
        assert_eq!(running.host.status().confirmed_publications, 1);
        // The host did not receive a settled-round acknowledgement, but the
        // receiver can prove the committed collision from its own disk owner.
        assert_eq!(running.host.status().confirmed_conflicts, 0);
        assert_eq!(running.client.status().confirmed_conflicts, 1);
        assert_eq!(running.client.status().state.conflicts_kept, 1);
        let files = destination.files();
        assert_eq!(files.len(), 2);
        assert!(files.values().any(|bytes| bytes == b"confirmed first"));
        assert_eq!(
            fs::read(destination.0.join("a-confirmed")).unwrap(),
            b"client original"
        );
        running.input_live(&cx);
        running.link.control_progress(&cx).await;
    });
}

#[test]
fn mutation_after_scan_cannot_publish_different_bytes_under_the_confirmed_source_version() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"version one").unwrap();
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        let until = clock(&cx) + 2_000_000;
        while running.host.status().state.phase != SyncPhase::Transferring {
            assert!(clock(&cx) < until);
            let (host, client) = running.step(&cx).await;
            host.unwrap();
            client.unwrap();
        }
        assert_eq!(running.sender.stage(), Stage::Idle);
        assert_eq!(running.host.status().state.entries_total, 1);
        // The scan has pinned this descriptor and hashed version one; the
        // ordinary ATP source has not prepared its own manifest yet.
        fs::write(source.0.join("selected"), b"version two").unwrap();
        loop {
            assert!(clock(&cx) < until);
            let (host, _) = running.step(&cx).await;
            if let Err(error) = host {
                assert_eq!(
                    error,
                    LiveError::Send(fr_files::sender::Error::SourceChanged)
                );
                break;
            }
        }
        assert_eq!(running.host.status().confirmed_publications, 0);
        assert!(running.receiver.last_result().is_none());
        assert!(destination.files().is_empty());
        running.retire();
        running.input_live(&cx);
    });
}

#[test]
fn a_full_initial_snapshot_leaves_budget_for_later_versions_on_the_same_lane() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        for index in 0..fr_files::sync::live::MAX_ENTRIES {
            fs::write(source.0.join(format!("item-{index:02}")), []).unwrap();
        }
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        running.settled(&cx, 1).await;
        assert_eq!(running.host.status().state.entries_completed, 32);
        assert_eq!(running.host.status().confirmed_publications, 32);
        assert_eq!(destination.files().len(), 32);
        fs::write(source.0.join("item-00"), b"a later source version").unwrap();
        running.settled(&cx, 2).await;
        let host = running.host.status();
        assert_eq!(host.failure, None);
        assert_eq!(host.state.entries_total, 1);
        assert_eq!(host.state.entries_completed, 1);
        assert_eq!(host.state.conflicts_kept, 1);
        assert_eq!(host.confirmed_publications, 33);
        assert_eq!(running.client.status().confirmed_publications, 33);
        assert_eq!(
            fs::read(destination.0.join("item-00")).unwrap(),
            [] as [u8; 0]
        );
        let files = destination.files();
        assert_eq!(files.len(), 33);
        assert!(
            files
                .values()
                .any(|bytes| bytes == b"a later source version")
        );
        running.input_live(&cx);
    });
}

#[test]
fn folder_offer_exceeding_its_announced_round_never_reaches_the_disk_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        oversized_round(&cx, false).await;
    });
}

#[test]
fn upload_offer_exceeding_its_announced_round_never_reaches_the_host_disk_worker() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        oversized_round(&cx, true).await;
    });
}

fn source_connection(link: &mut Link, upload: bool) -> &mut fr_transport::quic::QuicRecords {
    if upload { &mut link.c } else { &mut link.h }
}

fn destination_connection(link: &mut Link, upload: bool) -> &mut fr_transport::quic::QuicRecords {
    if upload { &mut link.h } else { &mut link.c }
}

#[allow(clippy::too_many_lines)] // Retain the complete original wire/admission/refusal scenario.
async fn oversized_round(cx: &Cx, upload: bool) {
    use fr_transport::quic::Disposition;
    use fr_wire::{
        files::{self, Body, Message, Reason},
        sync_job_state::{self, ConflictPolicy, SyncDirection, SyncJobState},
    };
    let mut link = Link::new(cx).await;
    let (host_lane, client_lane, input, _client_input, authority) = if upload {
        link.upload_sync(cx, true, true).await
    } else {
        link.download(cx, true).await
    };
    let destination = Directory::new();
    let (source_lane, mut client, mut receiver) = if upload {
        let (job, receiver) = receiver_owner(
            cx,
            &destination.0,
            41,
            &mut link.h,
            host_lane,
            Authority::from_input(&input),
            true,
        );
        (client_lane, job, receiver)
    } else {
        let (job, receiver) =
            controller(cx, &destination.0, 41, &mut link.c, client_lane, authority);
        (host_lane, job, receiver)
    };
    let until = clock(cx) + 2_000_000;
    let mut state = SyncJobState {
        job_id: 41,
        sync_direction: if upload {
            SyncDirection::ControllerToHost
        } else {
            SyncDirection::HostToController
        },
        conflict_policy: ConflictPolicy::KeepBoth,
        phase: SyncPhase::Advertised,
        reason: Reason::None,
        entries_total: 0,
        entries_completed: 0,
        conflicts_kept: 0,
        bytes_total: 0,
        bytes_completed: 0,
        manifest_revision: 0,
    };
    if upload {
        let mut advertised = false;
        while !advertised {
            assert!(clock(cx) < until);
            client.service(&mut receiver, &mut link.h, || true).unwrap();
            link.drive(cx).await;
            source_lane
                .dispatch(
                    &mut link.c,
                    cx,
                    || true,
                    |bytes| {
                        assert_eq!(
                            sync_job_state::decode(
                                bytes,
                                source_lane.incoming(),
                                source_lane.limits()
                            )
                            .unwrap(),
                            state
                        );
                        advertised = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        state.phase = SyncPhase::Accepted;
        source_lane
            .send_sync(&mut link.c, cx, &state, until, || true)
            .unwrap();
        while client.status().state != state || !source_lane.send_drained(&link.c).unwrap() {
            assert!(clock(cx) < until);
            link.drive(cx).await;
            client.service(&mut receiver, &mut link.h, || true).unwrap();
        }
    } else {
        source_lane
            .send_sync(
                source_connection(&mut link, upload),
                cx,
                &state,
                until,
                || true,
            )
            .unwrap();
        let mut accepted = false;
        while !accepted {
            assert!(clock(cx) < until);
            link.drive(cx).await;
            client
                .service(
                    &mut receiver,
                    destination_connection(&mut link, upload),
                    || true,
                )
                .unwrap();
            source_lane
                .dispatch(
                    source_connection(&mut link, upload),
                    cx,
                    || true,
                    |bytes| {
                        let acceptance = sync_job_state::decode(
                            bytes,
                            source_lane.incoming(),
                            source_lane.limits(),
                        )
                        .unwrap();
                        assert_eq!(acceptance.phase, SyncPhase::Accepted);
                        accepted = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
    }
    state.manifest_revision = 1;
    for phase in [SyncPhase::Scanning, SyncPhase::Transferring] {
        state.phase = phase;
        if phase == SyncPhase::Transferring {
            state.entries_total = 1;
            state.bytes_total = 3;
        }
        source_lane
            .send_sync(
                source_connection(&mut link, upload),
                cx,
                &state,
                until,
                || true,
            )
            .unwrap();
        while client.status().state != state
            || !source_lane
                .send_drained(source_connection(&mut link, upload))
                .unwrap()
        {
            assert!(clock(cx) < until);
            link.drive(cx).await;
            client
                .service(
                    &mut receiver,
                    destination_connection(&mut link, upload),
                    || true,
                )
                .unwrap();
        }
    }
    let offer = native_support::offered(&native_support::manifest("one-file", b"four"));
    let mut bytes = vec![0; source_lane.limits().record_bytes()];
    let n = files::encode(
        Message {
            id: 1,
            body: Body::Offer {
                profile: files::ATP_PORTABLE_FULL,
                atp: &offer,
            },
        },
        source_lane.outgoing(),
        source_lane.limits(),
        &mut bytes,
    )
    .unwrap();
    source_lane
        .send(
            source_connection(&mut link, upload),
            cx,
            &bytes[..n],
            until,
            || true,
        )
        .unwrap();
    let mut refused = false;
    loop {
        assert!(clock(cx) < until);
        if let Err(error) = client.service(
            &mut receiver,
            destination_connection(&mut link, upload),
            || true,
        ) {
            assert_eq!(error, LiveError::Resource);
            break;
        }
        link.drive(cx).await;
        source_lane
            .dispatch(
                source_connection(&mut link, upload),
                cx,
                || true,
                |bytes| {
                    let proof =
                        files::decode(bytes, source_lane.incoming(), source_lane.limits()).unwrap();
                    assert_eq!(
                        proof.body,
                        Body::Complete {
                            disposition: files::Disposition::Refused,
                            reason: Reason::Resource,
                            published_bytes: 0,
                            atp: &[],
                        }
                    );
                    refused = true;
                    Ok(Disposition::Consumed)
                },
            )
            .unwrap();
    }
    assert!(refused);
    loop {
        assert!(clock(cx) < until);
        if let Some(result) = receiver.try_finish_cleanup() {
            result.unwrap();
            break;
        }
        std::thread::yield_now();
    }
    assert!(receiver.collect_after_close().unwrap().is_none());
    assert!(destination.files().is_empty());
    assert_eq!(client.status().confirmed_publications, 0);
    link.control_progress(cx).await;
}

#[test]
fn idle_rounds_preserve_receipts_before_delivering_a_later_source_change() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"version one").unwrap();
        fs::write(source.0.join("unchanged"), b"stable source").unwrap();
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        running.settled(&cx, 1).await;
        assert_eq!(running.host.status().confirmed_publications, 2);
        let original_files = destination.files();

        // Cross the cache's periodic full-scan boundary with no new versions.
        // An idle scan must retain its cache without inventing publication receipts.
        for round in 2..=6 {
            running.settled(&cx, round).await;
            let host = running.host.status();
            assert_eq!(host.state.phase, SyncPhase::Settled);
            assert_eq!((host.state.entries_total, host.state.bytes_total), (0, 0));
            assert_eq!(host.confirmed_publications, 2);
            assert_eq!(running.client.status().confirmed_publications, 2);
            assert_eq!(destination.files(), original_files);
            running.input_live(&cx);
        }

        // A same-length edit invalidates the old manifest and delivers once.
        fs::write(source.0.join("selected"), b"version two").unwrap();
        running.settled(&cx, 7).await;
        let host = running.host.status();
        assert_eq!(host.state.entries_total, 1);
        assert_eq!(host.state.entries_completed, 1);
        assert_eq!(host.state.conflicts_kept, 1);
        assert_eq!(host.confirmed_publications, 3);
        assert_eq!(running.client.status().confirmed_publications, 3);
        assert_eq!(
            fs::read(destination.0.join("selected")).unwrap(),
            b"version one"
        );
        let files = destination.files();
        assert_eq!(files.len(), 3);
        assert!(files.values().any(|bytes| bytes == b"version two"));
        assert_eq!(
            fs::read(destination.0.join("unchanged")).unwrap(),
            b"stable source"
        );
        running.input_live(&cx);
        running.retire();
        assert!(running.host.status().cleanup_complete && running.client.status().cleanup_complete);
    });
}

#[test]
fn upload_jobs_preserve_host_files_and_deliver_changed_source_versions_on_both_lane_profiles() {
    for duplex in [false, true] {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let source = Directory::new();
            let destination = Directory::new();
            let large: Vec<u8> = (0..120_007)
                .map(|n| u8::try_from(n % 251).unwrap())
                .collect();
            fs::write(source.0.join("large.bin"), &large).unwrap();
            fs::write(source.0.join("empty"), []).unwrap();
            fs::write(source.0.join("report.txt"), b"controller version one").unwrap();
            fs::write(destination.0.join("report.txt"), b"original host document").unwrap();
            let mut running =
                Running::upload(&cx, &source.0, &destination.0, 41, duplex, true).await;
            running.settled(&cx, 1).await;
            assert_eq!(
                running.host.status().state.sync_direction,
                fr_wire::sync_job_state::SyncDirection::ControllerToHost
            );
            assert_eq!(
                running.host.status().state.phase,
                SyncPhase::ConflictRecorded
            );
            assert_eq!(running.host.status().confirmed_publications, 3);
            assert_eq!(running.client.status().confirmed_publications, 3);
            assert_eq!(running.host.status().confirmed_conflicts, 1);
            assert_eq!(fs::read(destination.0.join("large.bin")).unwrap(), large);
            assert_eq!(
                fs::read(destination.0.join("report.txt")).unwrap(),
                b"original host document"
            );
            assert!(
                destination
                    .files()
                    .values()
                    .any(|bytes| bytes == b"controller version one")
            );

            // Independent host edits/deletion must not cause replay of an
            // acknowledged controller version. New controller versions do.
            fs::write(destination.0.join("large.bin"), b"independent host edit").unwrap();
            fs::remove_file(destination.0.join("empty")).unwrap();
            fs::write(source.0.join("report.txt"), b"controller version two").unwrap();
            fs::write(source.0.join("added"), b"new controller file").unwrap();
            running.settled(&cx, 2).await;
            assert_eq!(running.host.status().state.entries_total, 2);
            assert_eq!(running.host.status().confirmed_publications, 5);
            assert_eq!(running.host.status().confirmed_conflicts, 2);
            assert_eq!(running.client.status().confirmed_conflicts, 2);
            assert_eq!(
                fs::read(destination.0.join("large.bin")).unwrap(),
                b"independent host edit"
            );
            assert!(!destination.0.join("empty").exists());
            assert_eq!(
                fs::read(destination.0.join("added")).unwrap(),
                b"new controller file"
            );
            let second = destination.files();
            assert!(
                second
                    .values()
                    .any(|bytes| bytes == b"controller version one")
            );
            assert!(
                second
                    .values()
                    .any(|bytes| bytes == b"controller version two")
            );
            running.settled(&cx, 3).await;
            assert_eq!(running.host.status().state.entries_total, 0);
            assert_eq!(running.host.status().confirmed_publications, 5);
            assert_eq!(destination.files(), second);
            running.input_live(&cx);
            running.retire();
            running.link.control_progress(&cx).await;
        });
    }
}

#[test]
fn an_upload_source_waits_for_host_advertisement_before_scanning_or_sending() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        // This source entry would fail a scan. Merely selecting the local
        // source cannot cause that scan before the original host advertises.
        std::os::unix::fs::symlink(&source.0, source.0.join("unselected")).unwrap();
        let mut running = Running::upload(&cx, &source.0, &destination.0, 41, false, true).await;
        let until = clock(&cx) + 100_000;
        while clock(&cx) < until {
            running
                .host
                .service(&mut running.sender, &mut running.link.c, || true)
                .unwrap();
            running.link.drive(&cx).await;
        }
        assert_eq!(running.host.status().state.manifest_revision, 0);
        assert_eq!(running.host.status().failure, None);
        assert_eq!(running.sender.stage(), Stage::Idle);
        assert!(running.receiver.last_result().is_none());
        assert!(destination.files().is_empty());
        fs::remove_file(source.0.join("unselected")).unwrap();
        fs::write(source.0.join("selected"), b"explicitly accepted source").unwrap();
        running.settled(&cx, 1).await;
        assert_eq!(
            fs::read(destination.0.join("selected")).unwrap(),
            b"explicitly accepted source"
        );
        running.input_live(&cx);
    });
}

#[test]
fn a_different_upload_job_id_never_creates_or_scans_a_host_job() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        std::os::unix::fs::symlink(&source.0, source.0.join("not-selected")).unwrap();
        let mut running = Running::upload(&cx, &source.0, &destination.0, 42, true, true).await;
        let until = clock(&cx) + 1_000_000;
        loop {
            assert!(clock(&cx) < until, "mismatched upload job was not refused");
            let (source, _) = running.step(&cx).await;
            if let Err(error) = source {
                assert_eq!(error, LiveError::WrongJob);
                break;
            }
        }
        assert_eq!(running.host.status().confirmed_publications, 0);
        assert_eq!(running.host.status().state.manifest_revision, 0);
        assert!(running.receiver.last_result().is_none());
        assert!(destination.files().is_empty());
        running.retire();
        running.input_live(&cx);
        running.link.control_progress(&cx).await;
    });
}

#[test]
fn upload_jobs_require_the_separate_positive_capability() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"no unnegotiated transfer").unwrap();
        let mut running = Running::upload(&cx, &source.0, &destination.0, 41, false, false).await;
        assert_eq!(
            running
                .host
                .service(&mut running.sender, &mut running.link.c, || true),
            Err(LiveError::NotNegotiated)
        );
        assert!(running.receiver.last_result().is_none());
        assert!(destination.files().is_empty());
        running.retire();
        running.input_live(&cx);
    });
}

#[test]
fn a_replacement_connection_cannot_resume_an_original_upload_job() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"original controller source").unwrap();
        let mut running = Running::upload(&cx, &source.0, &destination.0, 41, false, true).await;
        let mut replacement = Link::new(&cx).await;
        assert_eq!(
            running
                .host
                .service(&mut running.sender, &mut replacement.c, || true),
            Err(LiveError::WrongConnection)
        );
        assert!(matches!(
            running
                .client
                .service(&mut running.receiver, &mut replacement.h, || true),
            Err(LiveError::WrongConnection
                | LiveError::Receive(fr_files::quic::Error::WrongConnection))
        ));
        assert_eq!(running.host.status().failure, None);
        assert_eq!(running.client.status().failure, None);
        running.settled(&cx, 1).await;
        assert_eq!(
            fs::read(destination.0.join("selected")).unwrap(),
            b"original controller source"
        );
        replacement.h.close();
        replacement.c.close();
    });
}

#[test]
fn cancelling_an_upload_preserves_the_confirmed_prefix_and_kept_host_conflict() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("a-confirmed"), b"confirmed controller file").unwrap();
        fs::write(source.0.join("z-partial"), vec![0x53; 4 * 1024 * 1024]).unwrap();
        fs::write(destination.0.join("a-confirmed"), b"original host file").unwrap();
        let mut running = Running::upload(&cx, &source.0, &destination.0, 41, true, true).await;
        let until = clock(&cx) + 2_000_000;
        while running
            .receiver
            .progress()
            .is_none_or(|p| p.id != 2 || p.staged_bytes == 0)
        {
            assert!(clock(&cx) < until, "second upload file did not stage");
            let (source, destination) = running.step(&cx).await;
            source.unwrap();
            destination.unwrap();
        }
        assert_eq!(running.host.status().confirmed_publications, 1);
        assert!(!destination.0.join("z-partial").exists());
        running.retire();
        assert_eq!(running.host.status().failure, Some(LiveError::Cancelled));
        assert_eq!(running.host.status().confirmed_publications, 1);
        assert_eq!(running.host.status().rounds_settled, 0);
        assert_eq!(running.client.status().confirmed_conflicts, 1);
        assert_eq!(running.client.status().confirmed_publications, 1);
        let files = destination.files();
        assert_eq!(files.len(), 2);
        assert!(
            files
                .values()
                .any(|bytes| bytes == b"confirmed controller file")
        );
        assert_eq!(
            fs::read(destination.0.join("a-confirmed")).unwrap(),
            b"original host file"
        );
        running.input_live(&cx);
        running.link.control_progress(&cx).await;
    });
}

#[test]
fn mutation_after_upload_scan_cannot_publish_a_different_source_version() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        fs::write(source.0.join("selected"), b"version one").unwrap();
        let mut running = Running::upload(&cx, &source.0, &destination.0, 41, false, true).await;
        let until = clock(&cx) + 2_000_000;
        while running.host.status().state.phase != SyncPhase::Transferring {
            assert!(clock(&cx) < until);
            let (source, destination) = running.step(&cx).await;
            source.unwrap();
            destination.unwrap();
        }
        assert_eq!(running.sender.stage(), Stage::Idle);
        fs::write(source.0.join("selected"), b"version two").unwrap();
        loop {
            assert!(clock(&cx) < until);
            if let (Err(error), _) = running.step(&cx).await {
                assert_eq!(
                    error,
                    LiveError::Send(fr_files::sender::Error::SourceChanged)
                );
                break;
            }
        }
        assert_eq!(running.host.status().confirmed_publications, 0);
        assert!(running.receiver.last_result().is_none());
        assert!(destination.files().is_empty());
        running.retire();
        running.input_live(&cx);
    });
}

#[test]
fn larger_download_jobs_page_fairly_without_replaying_confirmed_versions() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        paged_job(&cx, false, false).await;
    });
}

#[test]
fn larger_upload_jobs_use_bounded_pages_on_legacy_and_duplex_lanes() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        for duplex in [false, true] {
            paged_job(&cx, true, duplex).await;
        }
    });
}

async fn paged_job(cx: &Cx, upload: bool, duplex: bool) {
    let source = Directory::new();
    let destination = Directory::new();
    let page = fr_files::sync::live::MAX_ENTRIES;
    let count = 2 * page + 5;
    for index in 0..count {
        fs::write(
            source.0.join(format!("item-{index:03}")),
            format!("original source version {index}"),
        )
        .unwrap();
    }
    let mut running = if upload {
        Running::upload(cx, &source.0, &destination.0, 41, duplex, true).await
    } else {
        Running::new(cx, &source.0, &destination.0, 41).await
    };
    running.settled(cx, 1).await;
    assert_eq!(running.host.status().state.entries_completed, page);
    assert_eq!(destination.files().len(), page as usize);

    // A newly changed early name must not displace the next page's original
    // versions. Independent destination edits never authorize a replay.
    fs::write(source.0.join("item-000"), b"updated early source").unwrap();
    fs::write(
        destination.0.join("item-001"),
        b"independent destination edit",
    )
    .unwrap();
    running.settled(cx, 2).await;
    assert_eq!(running.host.status().state.entries_completed, page);
    assert_eq!(running.host.status().confirmed_publications, 2 * page);
    assert_eq!(destination.files().len(), (2 * page) as usize);
    assert_eq!(
        fs::read(destination.0.join("item-000")).unwrap(),
        b"original source version 0"
    );
    for index in page..2 * page {
        assert_eq!(
            fs::read(destination.0.join(format!("item-{index:03}"))).unwrap(),
            format!("original source version {index}").as_bytes()
        );
    }

    running.settled(cx, 3).await;
    let status = running.host.status();
    assert_eq!(status.state.entries_total, 6);
    assert_eq!(status.state.entries_completed, 6);
    assert_eq!(status.confirmed_publications, count + 1);
    assert_eq!(status.confirmed_conflicts, 1);
    assert_eq!(running.client.status().confirmed_publications, count + 1);
    for index in 2 * page..count {
        assert_eq!(
            fs::read(destination.0.join(format!("item-{index:03}"))).unwrap(),
            format!("original source version {index}").as_bytes()
        );
    }
    let delivered = destination.files();
    assert_eq!(delivered.len(), (count + 1) as usize);
    assert!(
        delivered
            .values()
            .any(|bytes| bytes == b"updated early source")
    );
    assert_eq!(
        fs::read(destination.0.join("item-001")).unwrap(),
        b"independent destination edit"
    );

    running.settled(cx, 4).await;
    assert_eq!(running.host.status().state.entries_total, 0);
    assert_eq!(running.host.status().confirmed_publications, count + 1);
    assert_eq!(destination.files(), delivered);
    running.input_live(cx);
    running.retire();
    assert!(running.host.status().cleanup_complete);
    assert!(running.client.status().cleanup_complete);
    running.link.control_progress(cx).await;
}

#[test]
fn cancelling_between_pages_preserves_the_original_completed_page_only() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        let page = fr_files::sync::live::MAX_ENTRIES;
        for index in 0..page + 1 {
            fs::write(source.0.join(format!("item-{index:03}")), b"selected").unwrap();
        }
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        running.settled(&cx, 1).await;
        let committed = destination.files();
        assert_eq!(committed.len(), page as usize);
        running.retire();
        assert_eq!(running.host.status().failure, Some(LiveError::Cancelled));
        assert_eq!(running.host.status().confirmed_publications, page);
        assert_eq!(running.client.status().confirmed_publications, page);
        assert_eq!(running.host.status().rounds_settled, 1);
        assert_eq!(destination.files(), committed);
        assert!(!destination.0.join(format!("item-{page:03}")).exists());
        running.input_live(&cx);
        running.link.control_progress(&cx).await;
    });
}

#[test]
fn oversized_source_directory_refuses_before_any_page_is_offered() {
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let source = Directory::new();
        let destination = Directory::new();
        for index in 0..=fr_files::sync::live::MAX_SOURCE_ENTRIES {
            fs::write(source.0.join(format!("item-{index:03}")), []).unwrap();
        }
        let mut running = Running::new(&cx, &source.0, &destination.0, 41).await;
        let until = clock(&cx) + 2_000_000;
        loop {
            assert!(clock(&cx) < until, "oversized source was not refused");
            let (host, _) = running.step(&cx).await;
            if let Err(error) = host {
                assert_eq!(
                    error,
                    LiveError::Source(fr_files::sync::SyncError::EntryLimitExceeded)
                );
                break;
            }
        }
        assert_eq!(running.host.status().confirmed_publications, 0);
        assert_eq!(running.host.status().rounds_settled, 0);
        assert!(destination.files().is_empty());
        assert!(running.receiver.last_result().is_none());
        running.retire();
        running.input_live(&cx);
    });
}

#[path = "remote_folder_sync/recursive.rs"]
mod recursive;

#[path = "remote_folder_sync/source_authority.rs"]
mod source_authority;
