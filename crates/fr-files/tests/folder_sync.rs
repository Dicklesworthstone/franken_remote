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
    scan_directory, scan_directory_checked,
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
    assert_eq!(second.published.len(), 0);
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
    assert_eq!(third.published.len(), 0);
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
        assert!(
            Path::new(entry.published_name())
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        );
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

    // Reconciliation has delivered both versions. Divergent original names
    // must not create two more random copies (and echo the old copies) forever.
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
    .unwrap();
    assert!(second.published.is_empty());
    assert_eq!(second.bytes_transferred, 0);
    assert_eq!(fs::read_dir(host_dir.path()).unwrap().count(), 2);
    assert_eq!(fs::read_dir(ctrl_dir.path()).unwrap().count(), 2);

    // A genuinely new revision still transfers once, preserving all originals.
    fs::write(host_dir.path().join("report.txt"), b"host revision C").unwrap();
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
    .unwrap();
    assert_eq!(third.published.len(), 1);
    assert_eq!(third.published[0].leg, TransferLeg::HostToController);
    assert!(third.published[0].conflict_copy);
    assert_eq!(
        fs::read(ctrl_dir.path().join(third.published[0].published_name())).unwrap(),
        b"host revision C"
    );
    assert_eq!(
        fs::read(ctrl_dir.path().join("report.txt")).unwrap(),
        b"controller revision B"
    );

    // Deleting a received conflict copy is a local choice, not a request to
    // silently replay the already acknowledged version under another suffix.
    fs::remove_file(ctrl_dir.path().join(third.published[0].published_name())).unwrap();
    let fourth = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &third.baseline,
        4,
        || Ok(()),
    )
    .unwrap();
    assert!(fourth.published.is_empty());

    // Removing the occupied original name must not turn the same acknowledged
    // source version into a new, non-conflicting transfer that resurrects it.
    fs::remove_file(ctrl_dir.path().join("report.txt")).unwrap();
    let fifth = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &fourth.baseline,
        5,
        || Ok(()),
    )
    .unwrap();
    assert!(fifth.published.is_empty());
    assert!(!ctrl_dir.path().join("report.txt").exists());

    // A later source edit is new work and can use the now-empty original name.
    fs::write(host_dir.path().join("report.txt"), b"host revision D").unwrap();
    let sixth = execute_sync_round(
        &cfg,
        host_ctx,
        ctrl_ctx,
        host_dir.path(),
        &host_drop,
        ctrl_dir.path(),
        &ctrl_drop,
        &fifth.baseline,
        6,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(sixth.published.len(), 1);
    assert!(!sixth.published[0].conflict_copy);
    assert_eq!(
        fs::read(ctrl_dir.path().join("report.txt")).unwrap(),
        b"host revision D"
    );
}

#[test]
fn mid_transfer_authority_revocation_leaves_zero_partial_published_files() {
    let host_dir = TempSyncDir::new("revoke-host");
    let ctrl_dir = TempSyncDir::new("revoke-ctrl");

    // 256 KiB payload requires multiple 64 KiB chunks so authority revokes mid-stream.
    let payload = vec![0x5A_u8; 256 * 1024];
    fs::write(host_dir.path().join("a-marker.txt"), b"complete").unwrap();
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
            // The completed first publication proves both scans are over.
            // Allow several chunk checkpoints on the second file before
            // revoking; cancellation must clean its unpublished staging.
            if ctrl_dir.path().join("a-marker.txt").exists() {
                checks += 1;
            }
            if checks >= 10 {
                Err(SyncError::AuthorityRevoked)
            } else {
                Ok(())
            }
        },
    )
    .expect_err("mid-transfer revocation must abort cleanly");

    assert_eq!(failure.error, SyncError::AuthorityRevoked);
    assert!(!ctrl_dir.path().join("large.bin").exists());
    // Only the complete marker remains; the partial second file is cleaned.
    assert_eq!(fs::read_dir(ctrl_dir.path()).unwrap().count(), 1);
    assert_eq!(failure.published.len(), 1);
    assert_eq!(failure.published[0].original_name(), "a-marker.txt");

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

#[test]
fn private_staging_entries_cannot_bypass_the_scan_work_bound() {
    let dir = TempSyncDir::new("scan-entry-bound");
    for index in 0..32 {
        fs::write(dir.path().join(format!(".fr-part-{index:032x}")), b"").unwrap();
    }
    let mut checks = 0;
    let result = scan_directory_checked(
        dir.path(),
        1,
        SyncLimits {
            max_entries: 2,
            ..SyncLimits::default()
        },
        || {
            checks += 1;
            Ok(())
        },
    );
    assert_eq!(result, Err(SyncError::EntryLimitExceeded));
    // Enumeration must stop after the bounded prefix, not collect all entries
    // or spend work proportional to a directory's unadmitted suffix.
    assert!(checks <= 8, "unbounded scan checkpoints: {checks}");
}

#[test]
fn a_large_file_scan_observes_revocation_between_hashing_chunks() {
    let dir = TempSyncDir::new("scan-revoked");
    fs::write(dir.path().join("large.bin"), vec![7; 512 * 1024]).unwrap();
    let mut checks = 0;
    let result = scan_directory_checked(
        dir.path(),
        1,
        SyncLimits {
            max_chunk_bytes: 1024,
            ..SyncLimits::default()
        },
        || {
            checks += 1;
            if checks >= 20 {
                Err(SyncError::AuthorityRevoked)
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(result, Err(SyncError::AuthorityRevoked));
    assert_eq!(checks, 20);
}

#[test]
fn a_replaced_root_cannot_redirect_a_round_away_from_its_receive_owner() {
    let host = TempSyncDir::new("root-original");
    let controller = TempSyncDir::new("root-controller");
    let replacement = TempSyncDir::new("root-replacement");
    fs::write(replacement.path().join("unselected.txt"), b"do not send").unwrap();
    let host_drop = open_drop(host.path(), 1024, 4096);
    let controller_drop = open_drop(controller.path(), 1024, 4096);
    let (host_context, controller_context) = wire_contexts();
    let error = execute_sync_round(
        &SyncJobConfig {
            job_id: 110,
            direction: SyncDirection::HostToController,
            conflict_policy: WireConflictPolicy::KeepBoth,
            limits: SyncLimits::default(),
        },
        host_context,
        controller_context,
        replacement.path(),
        &host_drop,
        controller.path(),
        &controller_drop,
        &SyncBaseline::new(),
        1,
        || Ok(()),
    )
    .unwrap_err();
    assert_eq!(error.error, SyncError::RootChanged);
    assert!(error.published.is_empty());
    assert_eq!(fs::read_dir(controller.path()).unwrap().count(), 0);
}

#[test]
fn cancellation_retains_the_receipt_of_an_already_published_file() {
    let host = TempSyncDir::new("partial-host");
    let controller = TempSyncDir::new("partial-controller");
    fs::write(host.path().join("a-completed.txt"), b"committed effects").unwrap();
    fs::write(host.path().join("b-cancelled.txt"), b"must not publish").unwrap();
    let host_drop = open_drop(host.path(), 1024, 4096);
    let controller_drop = open_drop(controller.path(), 1024, 4096);
    let (host_context, controller_context) = wire_contexts();
    let failure = execute_sync_round(
        &SyncJobConfig {
            job_id: 111,
            direction: SyncDirection::HostToController,
            conflict_policy: WireConflictPolicy::KeepBoth,
            limits: SyncLimits::default(),
        },
        host_context,
        controller_context,
        host.path(),
        &host_drop,
        controller.path(),
        &controller_drop,
        &SyncBaseline::new(),
        1,
        || {
            if controller.path().join("a-completed.txt").exists() {
                Err(SyncError::AuthorityRevoked)
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.error, SyncError::AuthorityRevoked);
    assert_eq!(failure.published.len(), 1);
    let receipt = &failure.published[0];
    assert_eq!(receipt.original_name(), "a-completed.txt");
    assert_eq!(receipt.published_name(), "a-completed.txt");
    assert_eq!(receipt.size, 17);
    assert_eq!(
        fs::read(controller.path().join(receipt.published_name())).unwrap(),
        b"committed effects"
    );
    assert!(!controller.path().join("b-cancelled.txt").exists());
    let terminal = &failure.wire_states.last().unwrap().state;
    assert_eq!(terminal.phase, SyncPhase::Cancelled);
    assert_eq!(terminal.entries_completed, 1);
    assert_eq!(terminal.bytes_completed, receipt.size);
}

#[test]
fn a_path_replaced_after_pinning_does_not_redirect_directory_enumeration() {
    let container = TempSyncDir::new("scan-root-replaced");
    let selected = container.path().join("selected");
    let retained = container.path().join("retained");
    fs::create_dir(&selected).unwrap();
    fs::set_permissions(&selected, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(selected.join("original.txt"), b"selected source").unwrap();
    let mut checks = 0;
    let manifest = scan_directory_checked(&selected, 1, SyncLimits::default(), || {
        checks += 1;
        if checks == 2 {
            // The root is pinned, but directory enumeration has not begun.
            fs::rename(&selected, &retained).unwrap();
            fs::create_dir(&selected).unwrap();
            fs::write(selected.join("replacement.txt"), b"unselected source").unwrap();
        }
        Ok(())
    })
    .unwrap();
    assert!(manifest.entries().contains_key("original.txt"));
    assert!(!manifest.entries().contains_key("replacement.txt"));
}

#[test]
fn a_grown_source_is_not_published_as_its_old_manifest_prefix() {
    let host = TempSyncDir::new("grown-host");
    let controller = TempSyncDir::new("grown-controller");
    fs::write(host.path().join("a-first.txt"), b"published").unwrap();
    fs::write(host.path().join("b-growing.txt"), b"prefix").unwrap();
    let host_drop = open_drop(host.path(), 1024, 4096);
    let controller_drop = open_drop(controller.path(), 1024, 4096);
    let (host_context, controller_context) = wire_contexts();
    let mut changed = false;
    let failure = execute_sync_round(
        &SyncJobConfig {
            job_id: 112,
            direction: SyncDirection::HostToController,
            conflict_policy: WireConflictPolicy::KeepBoth,
            limits: SyncLimits::default(),
        },
        host_context,
        controller_context,
        host.path(),
        &host_drop,
        controller.path(),
        &controller_drop,
        &SyncBaseline::new(),
        1,
        || {
            if !changed && controller.path().join("a-first.txt").exists() {
                // Both manifests were already hashed. The old implementation
                // accepted this prefix's digest without checking the new EOF.
                fs::write(host.path().join("b-growing.txt"), b"prefix and later data").unwrap();
                changed = true;
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(changed);
    assert_eq!(failure.error, SyncError::SourceChanged);
    assert_eq!(failure.published.len(), 1);
    assert!(!controller.path().join("b-growing.txt").exists());
    assert_eq!(fs::read_dir(controller.path()).unwrap().count(), 1);
}

#[test]
fn a_fifo_source_is_refused_without_waiting_for_a_writer() {
    let dir = TempSyncDir::new("fifo-source");
    let root = fs::File::open(dir.path()).unwrap();
    rustix::fs::mkfifoat(
        &root,
        "pipe",
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
    )
    .unwrap();
    let path = dir.path().to_owned();
    let (send, receive) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        send.send(scan_directory(&path, 1, SyncLimits::default()))
            .unwrap();
    });
    assert_eq!(
        receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("FIFO open must not block"),
        Err(SyncError::UnsupportedEntryKind),
    );
    worker.join().unwrap();
}

#[test]
fn a_failed_conflict_round_returns_a_baseline_for_its_committed_prefix() {
    let host = TempSyncDir::new("conflict-resume-host");
    let controller = TempSyncDir::new("conflict-resume-controller");
    fs::write(host.path().join("a-conflict.txt"), b"host version").unwrap();
    fs::write(
        controller.path().join("a-conflict.txt"),
        b"controller version",
    )
    .unwrap();
    fs::write(host.path().join("b-next.txt"), b"next transfer").unwrap();
    let host_drop = open_drop(host.path(), 1024, 4096);
    let controller_drop = open_drop(controller.path(), 1024, 4096);
    let (host_context, controller_context) = wire_contexts();
    let config = SyncJobConfig {
        job_id: 113,
        direction: SyncDirection::Bidirectional,
        conflict_policy: WireConflictPolicy::KeepBoth,
        limits: SyncLimits::default(),
    };
    let failure = execute_sync_round(
        &config,
        host_context,
        controller_context,
        host.path(),
        &host_drop,
        controller.path(),
        &controller_drop,
        &SyncBaseline::new(),
        1,
        || {
            if fs::read_dir(controller.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".fr-conflict-")
            }) {
                Err(SyncError::AuthorityRevoked)
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.error, SyncError::AuthorityRevoked);
    assert_eq!(failure.published.len(), 1);
    assert!(failure.published[0].conflict_copy);
    let original_receipt = &failure.published[0];

    let resumed = execute_sync_round(
        &config,
        host_context,
        controller_context,
        host.path(),
        &host_drop,
        controller.path(),
        &controller_drop,
        &failure.baseline,
        2,
        || Ok(()),
    )
    .unwrap();
    assert_eq!(resumed.published.len(), 2);
    assert!(!resumed.published.iter().any(|entry| {
        entry.leg == original_receipt.leg
            && entry.original_name() == original_receipt.original_name()
    }));
    assert_eq!(fs::read_dir(host.path()).unwrap().count(), 3);
    assert_eq!(fs::read_dir(controller.path()).unwrap().count(), 3);
    let settled = execute_sync_round(
        &config,
        host_context,
        controller_context,
        host.path(),
        &host_drop,
        controller.path(),
        &controller_drop,
        &resumed.baseline,
        3,
        || Ok(()),
    )
    .unwrap();
    assert!(settled.published.is_empty());
}
