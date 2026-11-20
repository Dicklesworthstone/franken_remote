//! Real-filesystem integration tests for bounded, authority-fenced ATP folder
//! synchronization (`crates/fr-files/src/sync.rs`, `0x0075 SyncJobState`).

#![cfg(any(target_os = "linux", target_os = "macos"))]

use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_files::receive::{ConflictPolicy, DropDirectory, Limits as ReceiveLimits};
use fr_files::sync::{
    SyncBaseline, SyncError, SyncJobConfig, SyncLimits, TransferLeg, execute_sync_round,
    scan_directory,
};
use fr_wire::files::{
    Context as WireContext, Direction as WireDirection, Lane as WireLane, Limits as WireLimits,
    Reason as WireReason, Role,
};
use fr_wire::sync_job_state::{
    self, ConflictPolicy as WireConflictPolicy, SyncDirection, SyncPhase,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static DIR_COUNTER: AtomicU64 = AtomicU64::new(1);

struct TempSyncDir {
    path: PathBuf,
}

impl TempSyncDir {
    fn new(label: &str) -> Self {
        let id = DIR_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("fr-folder-sync-{label}-{pid}-{id}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create private temp dir");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).expect("chmod 0700");
        let canonical = fs::canonicalize(&path).expect("canonicalize temp dir");
        Self { path: canonical }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempSyncDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn wire_limits() -> WireLimits {
    WireLimits::new(&ProtocolLimits::ABSOLUTE, 4096).unwrap()
}

fn wire_contexts() -> (WireContext, WireContext) {
    let base = WireContext {
        session: RemoteSessionId::from_raw(u128::from_be_bytes([0x11; 16])),
        lease: InputLeaseId::from_raw(u128::from_be_bytes([0x22; 16])),
        handle: u128::from_be_bytes([0x33; 16]),
        channel: 42,
        sender: Role::Host,
        direction: WireDirection::ToHost,
        lane: WireLane::Files,
    };
    let ctrl = WireContext {
        sender: Role::Controller,
        ..base
    };
    (base, ctrl)
}

fn open_drop(path: &Path, max_file: u64, max_reserved: u64) -> DropDirectory {
    DropDirectory::open(
        path,
        ReceiveLimits {
            max_file_bytes: max_file,
            max_reserved_bytes: max_reserved,
            max_transfers: 8,
        },
    )
    .expect("open pinned drop directory")
    .with_conflict_policy(ConflictPolicy::KeepBoth)
}

#[test]
fn bidirectional_sync_transfers_both_ways_and_emits_valid_wire_records() {
    let host_dir = TempSyncDir::new("bidir-host");
    let ctrl_dir = TempSyncDir::new("bidir-ctrl");

    fs::write(host_dir.path().join("notes.txt"), b"from host workstation").unwrap();
    fs::write(ctrl_dir.path().join("draft.md"), b"from remote controller").unwrap();
    // Include an empty regular file to verify 0-byte manifest handling.
    fs::write(host_dir.path().join("empty.dat"), b"").unwrap();

    let host_drop = open_drop(host_dir.path(), 1024 * 1024, 4 * 1024 * 1024);
    let ctrl_drop = open_drop(ctrl_dir.path(), 1024 * 1024, 4 * 1024 * 1024);
    let (host_ctx, ctrl_ctx) = wire_contexts();

    let cfg = SyncJobConfig {
        job_id: 77,
        direction: SyncDirection::Bidirectional,
        conflict_policy: WireConflictPolicy::KeepBoth,
        limits: SyncLimits::default(),
    };

    let outcome = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &SyncBaseline::new(),
        1,
        || Ok(()),
    )
    .expect("bidirectional sync succeeds");

    assert_eq!(outcome.final_phase, SyncPhase::Settled);
    assert_eq!(outcome.conflicts_kept, 0);
    assert_eq!(outcome.published.len(), 3);
    assert_eq!(
        fs::read(ctrl_dir.path().join("notes.txt")).unwrap(),
        b"from host workstation"
    );
    assert_eq!(
        fs::read(host_dir.path().join("draft.md")).unwrap(),
        b"from remote controller"
    );
    assert_eq!(fs::read(ctrl_dir.path().join("empty.dat")).unwrap(), b"");

    // Verify every emitted 0x0075 SyncJobState wire frame decodes back through fr-wire.
    assert!(outcome.wire_states.len() >= 6);
    assert_eq!(outcome.wire_states[0].state.phase, SyncPhase::Advertised);
    assert_eq!(outcome.wire_states[1].state.phase, SyncPhase::Accepted);
    assert_eq!(outcome.wire_states[2].state.phase, SyncPhase::Scanning);
    assert_eq!(
        outcome.wire_states.last().unwrap().state.phase,
        SyncPhase::Settled
    );
    for emitted in &outcome.wire_states {
        let ctx = if emitted.sender == Role::Host {
            host_ctx
        } else {
            ctrl_ctx
        };
        let decoded = sync_job_state::decode(&emitted.frame, ctx, wire_limits())
            .expect("emitted 0x0075 wire frame must decode cleanly");
        assert_eq!(decoded, emitted.state);
    }

    // Running a second round with the returned baseline is a zero-transfer NoOp.
    let second = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &outcome.baseline,
        2,
        || Ok(()),
    )
    .expect("second round settles immediately");
    assert_eq!(second.final_phase, SyncPhase::Settled);
    assert!(second.published.is_empty());
    assert_eq!(second.bytes_transferred, 0);

    // Non-destructive deletion rule: deleting notes.txt on controller does NOT delete host's copy
    // and does NOT silently resurrect it on the next round because baseline remembers it.
    fs::remove_file(ctrl_dir.path().join("notes.txt")).unwrap();
    let third = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &second.baseline,
        3,
        || Ok(()),
    )
    .expect("local deletion stays non-destructive");
    assert!(third.published.is_empty());
    assert_eq!(
        fs::read(host_dir.path().join("notes.txt")).unwrap(),
        b"from host workstation"
    );
    assert!(!ctrl_dir.path().join("notes.txt").exists());
}

#[test]
fn concurrent_edits_preserve_both_versions_with_keep_both_and_report_conflict_phase() {
    let host_dir = TempSyncDir::new("conflict-host");
    let ctrl_dir = TempSyncDir::new("conflict-ctrl");

    fs::write(host_dir.path().join("report.txt"), b"host revision A").unwrap();
    fs::write(ctrl_dir.path().join("report.txt"), b"controller revision B").unwrap();

    let host_drop = open_drop(host_dir.path(), 1024 * 1024, 4 * 1024 * 1024);
    let ctrl_drop = open_drop(ctrl_dir.path(), 1024 * 1024, 4 * 1024 * 1024);
    let (host_ctx, ctrl_ctx) = wire_contexts();

    let cfg = SyncJobConfig {
        job_id: 99,
        direction: SyncDirection::Bidirectional,
        conflict_policy: WireConflictPolicy::KeepBoth,
        limits: SyncLimits::default(),
    };

    let outcome = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &SyncBaseline::new(),
        1,
        || Ok(()),
    )
    .expect("concurrent edit resolves via KeepBoth");

    assert_eq!(outcome.final_phase, SyncPhase::ConflictRecorded);
    assert_eq!(outcome.conflicts_kept, 2);
    assert_eq!(outcome.published.len(), 2);

    // Originals on BOTH machines remain byte-for-byte intact (zero silent overwrite).
    assert_eq!(
        fs::read(host_dir.path().join("report.txt")).unwrap(),
        b"host revision A"
    );
    assert_eq!(
        fs::read(ctrl_dir.path().join("report.txt")).unwrap(),
        b"controller revision B"
    );

    // Peer versions landed under bounded `.fr-conflict-...txt` names on each side.
    for entry in &outcome.published {
        assert!(entry.conflict_copy);
        assert!(entry.published_name().contains(".fr-conflict-"));
        assert!(entry.published_name().ends_with(".txt"));
        assert_ne!(entry.published_name(), "report.txt");
    }
    let host_conflict_name = outcome
        .published
        .iter()
        .find(|e| e.leg == TransferLeg::ControllerToHost)
        .unwrap()
        .published_name();
    let ctrl_conflict_name = outcome
        .published
        .iter()
        .find(|e| e.leg == TransferLeg::HostToController)
        .unwrap()
        .published_name();
    assert_eq!(
        fs::read(host_dir.path().join(host_conflict_name)).unwrap(),
        b"controller revision B"
    );
    assert_eq!(
        fs::read(ctrl_dir.path().join(ctrl_conflict_name)).unwrap(),
        b"host revision A"
    );

    // Final emitted wire state must decode cleanly as ConflictRecorded.
    let last = outcome.wire_states.last().unwrap();
    let decoded = sync_job_state::decode(&last.frame, host_ctx, wire_limits()).unwrap();
    assert_eq!(decoded.phase, SyncPhase::ConflictRecorded);
    assert_eq!(decoded.conflicts_kept, 2);
}

#[test]
fn mid_transfer_authority_revocation_leaves_zero_partial_published_files() {
    let host_dir = TempSyncDir::new("revoke-host");
    let ctrl_dir = TempSyncDir::new("revoke-ctrl");

    // 256 KiB payload requires multiple 64 KiB chunks so authority revokes mid-stream.
    let payload = vec![0x5A_u8; 256 * 1024];
    fs::write(host_dir.path().join("large.bin"), &payload).unwrap();

    let host_drop = open_drop(host_dir.path(), 1024 * 1024, 4 * 1024 * 1024);
    let ctrl_drop = open_drop(ctrl_dir.path(), 1024 * 1024, 4 * 1024 * 1024);
    let (host_ctx, ctrl_ctx) = wire_contexts();

    let cfg = SyncJobConfig {
        job_id: 105,
        direction: SyncDirection::HostToController,
        conflict_policy: WireConflictPolicy::KeepBoth,
        limits: SyncLimits::default(),
    };

    let mut checks = 0u32;
    let failure = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &SyncBaseline::new(),
        1,
        || {
            checks += 1;
            // Allow initial check + transfer start + 2 chunks, then revoke before completion.
            if checks >= 5 {
                Err(SyncError::AuthorityRevoked)
            } else {
                Ok(())
            }
        },
    )
    .expect_err("mid-transfer revocation must abort cleanly");

    assert_eq!(failure.error, SyncError::AuthorityRevoked);
    assert!(!ctrl_dir.path().join("large.bin").exists());
    // Staging file must also be cleaned up: controller directory stays completely empty.
    assert_eq!(fs::read_dir(ctrl_dir.path()).unwrap().count(), 0);

    let last = failure
        .wire_states
        .last()
        .expect("terminal cancelled wire state");
    let decoded = sync_job_state::decode(&last.frame, host_ctx, wire_limits()).unwrap();
    assert_eq!(decoded.phase, SyncPhase::Cancelled);
    assert_eq!(decoded.reason, WireReason::Permission);
}

#[test]
fn scanner_refuses_symlinks_subdirectories_and_quota_breaches() {
    let symlink_dir = TempSyncDir::new("symlink");
    let secret = TempSyncDir::new("outside-secret");
    let secret_file = secret.path().join("secret.txt");
    fs::write(&secret_file, b"outside root").unwrap();
    std::os::unix::fs::symlink(&secret_file, symlink_dir.path().join("escape.txt")).unwrap();

    assert_eq!(
        scan_directory(symlink_dir.path(), 1, SyncLimits::default()),
        Err(SyncError::SymlinkForbidden)
    );

    let subdir_root = TempSyncDir::new("subdir");
    fs::create_dir(subdir_root.path().join("nested")).unwrap();
    assert_eq!(
        scan_directory(subdir_root.path(), 1, SyncLimits::default()),
        Err(SyncError::SubdirectoryUnsupported)
    );

    let quota_root = TempSyncDir::new("quota");
    fs::write(quota_root.path().join("too_big.bin"), vec![1u8; 4096]).unwrap();
    let tight = SyncLimits {
        max_entries: 4,
        max_file_bytes: 1024,
        max_total_bytes: 2048,
        max_chunk_bytes: 1024,
    };
    assert_eq!(
        scan_directory(quota_root.path(), 1, tight),
        Err(SyncError::FileSizeLimitExceeded)
    );
}

#[test]
fn debug_formatting_redacts_filenames_and_digests() {
    let dir = TempSyncDir::new("redact");
    fs::write(
        dir.path().join("super-secret-payroll-2026.csv"),
        b"alice,999999",
    )
    .unwrap();
    let manifest = scan_directory(dir.path(), 1, SyncLimits::default()).unwrap();
    let rendered = format!("{manifest:?}");
    assert!(!rendered.contains("super-secret-payroll-2026"));
    assert!(!rendered.contains("alice"));
    let entry = manifest
        .entries()
        .get("super-secret-payroll-2026.csv")
        .unwrap();
    let entry_dbg = format!("{entry:?}");
    assert!(!entry_dbg.contains("super-secret-payroll-2026"));
    assert!(!entry_dbg.contains(entry.sha256_hex()));
}
