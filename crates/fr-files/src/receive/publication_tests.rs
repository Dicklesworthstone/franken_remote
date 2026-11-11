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
