//! Projected `KeepBoth` destinations must fit the next unchanged bounded scan.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use fr_core::ids::{InputLeaseId, RemoteSessionId};
use fr_files::{
    receive::{DropDirectory, Limits as ReceiveLimits},
    sync::{
        PublishedSyncEntry, SyncBaseline, SyncError, SyncJobConfig, SyncLimits, SyncRoundFailure,
        SyncRoundOutcome, execute_sync_round, scan_directory,
    },
};
use fr_wire::{
    files::{Context, Direction, Lane, Reason, Role},
    sync_job_state::{ConflictPolicy, SyncDirection, SyncPhase},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

struct Pair {
    host: PathBuf,
    controller: PathBuf,
    host_drop: DropDirectory,
    controller_drop: DropDirectory,
}

impl Pair {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!(
                "fr-sync-projection-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let host = root.join("host");
        let controller = root.join("controller");
        for path in [&host, &controller] {
            fs::create_dir(path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let open = |path: &Path| {
            DropDirectory::open(
                path,
                ReceiveLimits {
                    max_file_bytes: 1024,
                    max_reserved_bytes: 4096,
                    max_transfers: 4,
                },
            )
            .unwrap()
        };
        let host_drop = open(&host);
        let controller_drop = open(&controller);
        Self {
            host,
            controller,
            host_drop,
            controller_drop,
        }
    }

    fn round(
        &self,
        limits: SyncLimits,
        baseline: &SyncBaseline,
        revision: u64,
    ) -> Result<SyncRoundOutcome, SyncRoundFailure> {
        let config = SyncJobConfig {
            job_id: 201,
            direction: SyncDirection::Bidirectional,
            conflict_policy: ConflictPolicy::KeepBoth,
            limits,
        };
        let host = Context {
            session: RemoteSessionId::from_raw(0x11),
            lease: InputLeaseId::from_raw(0x22),
            handle: 0x33,
            channel: 42,
            sender: Role::Host,
            direction: Direction::ToHost,
            lane: Lane::Files,
        };
        let controller = Context {
            sender: Role::Controller,
            ..host
        };
        execute_sync_round(
            &config,
            host,
            controller,
            &self.host,
            &self.host_drop,
            &self.controller,
            &self.controller_drop,
            baseline,
            revision,
            || Ok(()),
        )
    }
}

fn limits(entries: u32, bytes: u64) -> SyncLimits {
    SyncLimits {
        max_entries: entries,
        max_file_bytes: 4,
        max_total_bytes: bytes,
        max_chunk_bytes: 4,
    }
}

fn assert_refused_before_publication(failure: &SyncRoundFailure, error: SyncError) {
    assert_eq!(failure.error, error);
    assert_eq!(failure.published, [] as [PublishedSyncEntry; 0]);
    assert_eq!(failure.baseline, SyncBaseline::new());
    let terminal = &failure.wire_states.last().unwrap().state;
    assert_eq!(terminal.phase, SyncPhase::Failed);
    assert_eq!(terminal.reason, Reason::Resource);
    assert_eq!(terminal.entries_completed, 0);
    assert_eq!(terminal.bytes_completed, 0);
}

#[test]
fn two_individually_full_roots_are_refused_before_either_grows() {
    let pair = Pair::new();
    fs::write(pair.host.join("a.txt"), b"host").unwrap();
    fs::write(pair.controller.join("b.txt"), b"peer").unwrap();
    let failure = pair
        .round(limits(1, 8), &SyncBaseline::new(), 1)
        .unwrap_err();
    assert_refused_before_publication(&failure, SyncError::EntryLimitExceeded);
    assert_eq!(fs::read_dir(&pair.host).unwrap().count(), 1);
    assert_eq!(fs::read_dir(&pair.controller).unwrap().count(), 1);
    assert_eq!(fs::read(pair.host.join("a.txt")).unwrap(), b"host");
    assert_eq!(fs::read(pair.controller.join("b.txt")).unwrap(), b"peer");
}

#[test]
fn combined_destination_bytes_are_checked_before_any_copy() {
    let pair = Pair::new();
    fs::write(pair.host.join("a.txt"), b"one").unwrap();
    fs::write(pair.controller.join("b.txt"), b"two").unwrap();
    let failure = pair
        .round(limits(4, 5), &SyncBaseline::new(), 1)
        .unwrap_err();
    assert_refused_before_publication(&failure, SyncError::TotalBytesLimitExceeded);
    assert_eq!(fs::read_dir(&pair.host).unwrap().count(), 1);
    assert_eq!(fs::read_dir(&pair.controller).unwrap().count(), 1);
}

#[test]
fn a_later_leg_overflow_includes_staging_and_refuses_the_whole_plan() {
    let pair = Pair::new();
    fs::write(pair.host.join("z.txt"), b"host").unwrap();
    fs::write(pair.controller.join("a.txt"), b"peer").unwrap();
    let staging = pair.controller.join(".fr-part-existing");
    fs::write(&staging, b"private pending bytes").unwrap();
    // The sorted plan first sends a.txt to the host, which has room. Its
    // later z.txt copy would make the controller's next scan exceed two.
    let failure = pair
        .round(limits(2, 8), &SyncBaseline::new(), 1)
        .unwrap_err();
    assert_refused_before_publication(&failure, SyncError::EntryLimitExceeded);
    assert_eq!(fs::read_dir(&pair.host).unwrap().count(), 1);
    assert_eq!(fs::read_dir(&pair.controller).unwrap().count(), 2);
    assert_eq!(fs::read(staging).unwrap(), b"private pending bytes");
}

#[test]
fn exact_entry_and_byte_bounds_allow_conflicts_and_an_unchanged_next_round() {
    let pair = Pair::new();
    fs::write(pair.host.join("same.txt"), b"one").unwrap();
    fs::write(pair.controller.join("same.txt"), b"two").unwrap();
    let limits = limits(2, 6);
    let outcome = pair.round(limits, &SyncBaseline::new(), 1).unwrap();
    assert_eq!(outcome.final_phase, SyncPhase::ConflictRecorded);
    assert_eq!(outcome.published.len(), 2);
    assert!(
        outcome
            .published
            .iter()
            .all(|receipt| receipt.conflict_copy)
    );
    assert_eq!(outcome.bytes_transferred, 6);
    for root in [&pair.host, &pair.controller] {
        let manifest = scan_directory(root, 2, limits).unwrap();
        assert_eq!(manifest.entries().len(), 2);
        assert_eq!(manifest.total_bytes(), 6);
    }
    let settled = pair.round(limits, &outcome.baseline, 2).unwrap();
    assert_eq!(settled.final_phase, SyncPhase::Settled);
    assert_eq!(settled.published, [] as [PublishedSyncEntry; 0]);
    assert_eq!(settled.bytes_transferred, 0);
    assert_eq!(fs::read(pair.host.join("same.txt")).unwrap(), b"one");
    assert_eq!(fs::read(pair.controller.join("same.txt")).unwrap(), b"two");
}
