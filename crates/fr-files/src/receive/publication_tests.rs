//! Conflict exhaustion does not depend on visible staging names.
#![cfg(target_os = "linux")]
use super::{DropDirectory, Error, Limits, Publication};
use asupersync::atp::object::ContentId;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "fr-files-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        Self(root)
    }
    fn open(&self) -> DropDirectory {
        DropDirectory::open(
            &self.0,
            Limits {
                max_file_bytes: 1024 * 1024,
                max_reserved_bytes: 1024 * 1024,
                max_transfers: 2,
            },
        )
        .unwrap()
    }
    fn entries(&self) -> usize {
        fs::read_dir(&self.0).unwrap().count()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

use super::{ConflictPolicy, PendingFile};

fn verified(root: &DropDirectory, name: &str, data: &[u8]) -> PendingFile {
    let mut file = root
        .begin(name, data.len() as u64, ContentId::from_bytes(data))
        .unwrap();
    if !data.is_empty() {
        file.write_chunk(0, data).unwrap();
    }
    file.verify().unwrap();
    file
}
#[test]
fn exhausted_conflict_candidates_fail_bounded_without_changing_existing_entries() {
    let scratch = Scratch::new();
    let root = scratch
        .open()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    let pending = verified(&root, "report.txt", b"incoming");
    let nonce = format!("{:032x}", pending.nonce);
    fs::write(scratch.0.join("report.txt"), b"keep").unwrap();
    for attempt in 0..8 {
        fs::write(
            scratch
                .0
                .join(format!("report.fr-conflict-{nonce}-{attempt}.txt")),
            b"keep",
        )
        .unwrap();
    }
    assert!(matches!(pending.publish_named(), Err(Error::Conflict)));
    assert_eq!(scratch.entries(), 9);
    for entry in fs::read_dir(&scratch.0).unwrap() {
        assert_eq!(fs::read(entry.unwrap().path()).unwrap(), b"keep");
    }
    // Failed publication cleanup releases only this staging owner's charge.
    assert_eq!(
        verified(&root, "other", b"new").publish(),
        Ok(Publication::Durable)
    );
}

#[test]
fn checked_publication_reclaims_staging_when_a_conflict_retry_loses_permission() {
    let scratch = Scratch::new();
    let root = scratch
        .open()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    fs::write(scratch.0.join("item"), b"existing").unwrap();
    let pending = verified(&root, "item", b"private incoming");
    let mut calls = 0;
    let result = pending.publish_named_checked(|| {
        calls += 1;
        if calls == 2 {
            Err(Error::Retired)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::Retired)));
    assert_eq!(calls, 2);
    assert_eq!(scratch.entries(), 1);
    assert_eq!(fs::read(scratch.0.join("item")).unwrap(), b"existing");
    assert_eq!(
        verified(&root, "other", b"x").publish(),
        Ok(Publication::Durable)
    );
}

#[test]
fn first_publication_refusal_has_no_effect_and_success_has_no_post_commit_gate() {
    let scratch = Scratch::new();
    let root = scratch.open();
    assert!(matches!(
        verified(&root, "item", b"secret").publish_named_checked(|| Err(Error::Retired)),
        Err(Error::Retired)
    ));
    assert_eq!(scratch.entries(), 0);
    let mut calls = 0;
    let published = verified(&root, "item", b"secret")
        .publish_named_checked(|| {
            calls += 1;
            if calls == 1 {
                Ok(())
            } else {
                Err(Error::Retired)
            }
        })
        .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(published.publication(), Publication::Durable);
    assert_eq!(fs::read(scratch.0.join("item")).unwrap(), b"secret");
}

#[test]
fn sync_retains_committed_receipts_and_original_cleanup_failure_after_revocation() {
    sync_cleanup_failure(false);
    sync_cleanup_failure(true);
}

#[allow(clippy::too_many_lines)]
fn sync_cleanup_failure(supervised: bool) {
    use crate::sync::{
        PublishedSyncEntry, SyncBaseline, SyncError, SyncJobConfig, SyncLimits, execute_sync_round,
    };
    use fr_core::ids::{InputLeaseId, RemoteSessionId};
    use fr_wire::{
        files::{Context, Direction, Lane, Reason, Role},
        sync_job_state::{ConflictPolicy as WireConflictPolicy, SyncDirection, SyncPhase},
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let host_dir = Scratch::new();
    let controller_dir = Scratch::new();
    fs::write(host_dir.0.join("a-saved.txt"), b"committed").unwrap();
    fs::write(host_dir.0.join("b-pending.txt"), b"not yet committed").unwrap();
    let host = host_dir.open();
    let controller = controller_dir.open();
    let controller = if supervised {
        controller.with_cleanup_status(super::CleanupStatus::default())
    } else {
        controller
    };
    let context = Context {
        session: RemoteSessionId::from_raw(11),
        lease: InputLeaseId::from_raw(12),
        handle: 13,
        channel: 42,
        sender: Role::Host,
        direction: Direction::ToHost,
        lane: Lane::Files,
    };
    let peer = Context {
        sender: Role::Controller,
        ..context
    };
    let config = SyncJobConfig {
        job_id: 19,
        direction: SyncDirection::HostToController,
        conflict_policy: WireConflictPolicy::KeepBoth,
        limits: SyncLimits::default(),
    };
    let failure = execute_sync_round(
        &config,
        context,
        peer,
        &host_dir.0,
        &host,
        &controller_dir.0,
        &controller,
        &SyncBaseline::new(),
        1,
        || {
            // Plant a real accounting failure only after the first commit and
            // while the next staging owner holds its reservation. This avoids
            // depending on Linux's private O_TMPFILE having a visible name.
            let receiving = controller.0.reserved.lock().unwrap().transfers > 0;
            if controller_dir.0.join("a-saved.txt").exists() && receiving {
                assert!(
                    catch_unwind(AssertUnwindSafe(|| {
                        let _held = controller.0.reserved.lock().unwrap();
                        panic!("planted original reservation accounting failure");
                    }))
                    .is_err()
                );
                Err(SyncError::AuthorityRevoked)
            } else {
                Ok(())
            }
        },
    )
    .unwrap_err();
    assert_eq!(failure.error, SyncError::AuthorityRevoked);
    assert!(failure.cleanup_failed);
    assert!(controller.cleanup_failed());
    assert_eq!(failure.published.len(), 1);
    assert_eq!(failure.published[0].published_name(), "a-saved.txt");
    assert!(failure.baseline.hash_for("a-saved.txt").is_some());
    assert_eq!(
        fs::read(controller_dir.0.join("a-saved.txt")).unwrap(),
        b"committed"
    );
    assert!(!controller_dir.0.join("b-pending.txt").exists());
    let terminal = &failure.wire_states.last().unwrap().state;
    assert_eq!(terminal.phase, SyncPhase::Failed);
    assert_eq!(terminal.reason, Reason::UnknownEffect);
    assert_eq!(terminal.entries_completed, 1);
    assert_eq!(terminal.bytes_completed, 9);
    let held = controller
        .0
        .reserved
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(held.transfers, 1, "failed cleanup cannot replenish quota");
    drop(held);

    // Cloning a new round cannot replace the supervising owner's failed witness.
    let mut admitted = false;
    let retry = execute_sync_round(
        &config,
        context,
        peer,
        &host_dir.0,
        &host,
        &controller_dir.0,
        &controller,
        &failure.baseline,
        2,
        || {
            admitted = true;
            Ok(())
        },
    )
    .unwrap_err();
    assert!(!admitted);
    assert_eq!(retry.error, SyncError::Receive(Error::Retired));
    assert!(retry.cleanup_failed);
    assert_eq!(retry.published, [] as [PublishedSyncEntry; 0]);
    assert_eq!(retry.baseline, failure.baseline);
}
