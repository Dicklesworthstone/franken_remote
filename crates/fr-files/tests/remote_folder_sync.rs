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
    sync::{ControllerJob, HostConfig, HostJob, LiveError},
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
    let client = ControllerJob::new(accepted_id, directory).unwrap();
    let receiver = Receiver::spawn_with_authority(
        cx.clone(),
        client_connection,
        client_lane,
        authority,
        Configuration {
            directory: client.directory(),
            permission: Permission::new(true),
            policy: Policy::conservative(),
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
    // The receiver's opaque authority must retain THIS accepted owner.
    _client_input: InputClient,
    cleaned: bool,
}
impl Running {
    async fn new(cx: &Cx, source: &Path, destination: &Path, accepted_id: u64) -> Self {
        let mut link = Link::new(cx).await;
        let (host_lane, client_lane, input, client_input, authority) =
            link.download(cx, true).await;
        let source = HostConfig::open(source, 41)
            .unwrap()
            .with_scan_interval(Duration::from_millis(100))
            .unwrap();
        let host = HostJob::new(
            source,
            cx.clone(),
            Authority::from_input(&input),
            Permission::new(true),
            &link.h,
        )
        .unwrap();
        let sender = Sender::owning(cx.clone(), &link.h, host_lane, SendPolicy::default()).unwrap();
        let (client, receiver) = controller(
            cx,
            destination,
            accepted_id,
            &mut link.c,
            client_lane,
            authority,
        );
        Self {
            link,
            host,
            sender,
            client,
            receiver,
            input,
            _client_input: client_input,
            cleaned: false,
        }
    }
    async fn step(&mut self, cx: &Cx) -> (Result<(), LiveError>, Result<(), LiveError>) {
        let host = self
            .host
            .service(&mut self.sender, &mut self.link.h, || true);
        let client = self
            .client
            .service(&mut self.receiver, &mut self.link.c, || true);
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
            let (host, client) = self.step(cx).await;
            assert_eq!(host, Ok(()), "host status: {:?}", self.host.status());
            assert_eq!(client, Ok(()), "client status: {:?}", self.client.status());
        }
    }
    fn retire(&mut self) {
        if self.cleaned {
            return;
        }
        self.host.stop();
        self.sender.cancel(&mut self.link.h).unwrap();
        self.client.stop();
        self.receiver.retire(&mut self.link.c).unwrap();
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
            b"confirmed first"
        );
        assert!(!destination.0.join("z-partial").exists());
        running.retire();
        assert_eq!(running.host.status().failure, Some(LiveError::Cancelled));
        assert_eq!(running.host.status().rounds_settled, 0);
        assert_eq!(running.host.status().confirmed_publications, 1);
        assert_eq!(destination.files().len(), 1);
        assert_eq!(
            fs::read(destination.0.join("a-confirmed")).unwrap(),
            b"confirmed first"
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
    use fr_transport::quic::Disposition;
    use fr_wire::{
        files::{self, Body, Message, Reason},
        sync_job_state::{self, ConflictPolicy, SyncDirection, SyncJobState},
    };

    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut link = Link::new(&cx).await;
        let (host_lane, client_lane, _input, _client_input, authority) =
            link.download(&cx, true).await;
        let destination = Directory::new();
        let (mut client, mut receiver) =
            controller(&cx, &destination.0, 41, &mut link.c, client_lane, authority);
        let until = clock(&cx) + 2_000_000;
        let mut state = SyncJobState {
            job_id: 41,
            sync_direction: SyncDirection::HostToController,
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
        host_lane
            .send_sync(&mut link.h, &cx, &state, until, || true)
            .unwrap();
        let mut accepted = false;
        while !accepted {
            assert!(clock(&cx) < until);
            link.drive(&cx).await;
            client.service(&mut receiver, &mut link.c, || true).unwrap();
            host_lane
                .dispatch(
                    &mut link.h,
                    &cx,
                    || true,
                    |bytes| {
                        let acceptance =
                            sync_job_state::decode(bytes, host_lane.incoming(), host_lane.limits())
                                .unwrap();
                        assert_eq!(acceptance.phase, SyncPhase::Accepted);
                        accepted = true;
                        Ok(Disposition::Consumed)
                    },
                )
                .unwrap();
        }
        state.manifest_revision = 1;
        for phase in [SyncPhase::Scanning, SyncPhase::Transferring] {
            state.phase = phase;
            if phase == SyncPhase::Transferring {
                state.entries_total = 1;
                state.bytes_total = 3;
            }
            host_lane
                .send_sync(&mut link.h, &cx, &state, until, || true)
                .unwrap();
            while client.status().state != state || !host_lane.send_drained(&link.h).unwrap() {
                assert!(clock(&cx) < until);
                link.drive(&cx).await;
                client.service(&mut receiver, &mut link.c, || true).unwrap();
            }
        }
        let offer = native_support::offered(&native_support::manifest("one-file", b"four"));
        let mut bytes = vec![0; host_lane.limits().record_bytes()];
        let n = files::encode(
            Message {
                id: 1,
                body: Body::Offer {
                    profile: files::ATP_PORTABLE_FULL,
                    atp: &offer,
                },
            },
            host_lane.outgoing(),
            host_lane.limits(),
            &mut bytes,
        )
        .unwrap();
        host_lane
            .send(&mut link.h, &cx, &bytes[..n], until, || true)
            .unwrap();
        let mut refused = false;
        loop {
            assert!(clock(&cx) < until);
            if let Err(error) = client.service(&mut receiver, &mut link.c, || true) {
                assert_eq!(error, LiveError::Resource);
                break;
            }
            link.drive(&cx).await;
            host_lane
                .dispatch(
                    &mut link.h,
                    &cx,
                    || true,
                    |bytes| {
                        let proof =
                            files::decode(bytes, host_lane.incoming(), host_lane.limits()).unwrap();
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
            assert!(clock(&cx) < until);
            if let Some(result) = receiver.try_finish_cleanup() {
                result.unwrap();
                break;
            }
            std::thread::yield_now();
        }
        assert!(receiver.collect_after_close().unwrap().is_none());
        assert!(destination.files().is_empty());
        assert_eq!(client.status().confirmed_publications, 0);
        link.control_progress(&cx).await;
    });
}
