//! Deterministic collision exhaustion without exposing private staging identity.
use super::*;
use std::{
    fs as stdfs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fr-dir-conflict-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        stdfs::create_dir(&path).unwrap();
        stdfs::set_permissions(&path, stdfs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
    fn root(&self) -> super::super::DropDirectory {
        super::super::DropDirectory::open(
            &self.0,
            super::super::Limits {
                max_file_bytes: 4,
                max_reserved_bytes: 4,
                max_transfers: 1,
            },
        )
        .unwrap()
        .with_conflict_policy(ConflictPolicy::KeepBoth)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        stdfs::remove_dir_all(&self.0).unwrap();
    }
}
fn empty_manifest() -> DirectoryManifest {
    DirectoryManifest::new(
        "project".into(),
        0,
        vec![],
        flat_merkle_root_from_digests(&[]),
    )
    .unwrap()
}

#[test]
fn colliding_candidates_preserve_every_kind_of_existing_entry() {
    let scratch = Scratch::new();
    let mut pending = scratch.root().begin_directory(empty_manifest()).unwrap();
    pending.verify().unwrap();
    stdfs::write(scratch.0.join("project"), b"original").unwrap();
    let candidates: Vec<_> = (0..4)
        .map(|i| scratch.0.join(conflict_name("project", pending.nonce, i)))
        .collect();
    stdfs::create_dir(&candidates[0]).unwrap();
    stdfs::write(candidates[0].join("precious"), b"directory").unwrap();
    stdfs::write(&candidates[1], b"file").unwrap();
    symlink("missing", &candidates[2]).unwrap();
    stdfs::hard_link(scratch.0.join("project"), &candidates[3]).unwrap();
    let expected = conflict_name("project", pending.nonce, 4);
    let result = pending.publish_named().unwrap();
    assert_eq!(result.name(), expected);
    assert!(result.was_renamed());
    assert_eq!(result.publication(), Publication::Durable);
    assert!(scratch.0.join(result.name()).is_dir());
    assert_eq!(stdfs::read(scratch.0.join("project")).unwrap(), b"original");
    assert_eq!(
        stdfs::read(candidates[0].join("precious")).unwrap(),
        b"directory"
    );
    assert_eq!(stdfs::read(&candidates[1]).unwrap(), b"file");
    assert_eq!(
        stdfs::read_link(&candidates[2]).unwrap(),
        PathBuf::from("missing")
    );
    assert_eq!(stdfs::read(&candidates[3]).unwrap(), b"original");
    assert_eq!(stdfs::read_dir(&scratch.0).unwrap().count(), 6);
}

#[test]
fn exhausted_candidates_are_terminal_and_release_only_owned_staging() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let mut pending = root.begin_directory(empty_manifest()).unwrap();
    pending.verify().unwrap();
    stdfs::write(scratch.0.join("project"), b"original").unwrap();
    let candidates: Vec<_> = (0..MAX_CONFLICT_ATTEMPTS)
        .map(|i| {
            let path = scratch.0.join(conflict_name("project", pending.nonce, i));
            stdfs::write(&path, [i]).unwrap();
            path
        })
        .collect();
    assert!(matches!(
        root.begin_directory(empty_manifest()),
        Err(Error::Quota)
    ));
    assert_eq!(pending.publish_named().unwrap_err(), Error::Conflict);
    assert_eq!(stdfs::read(scratch.0.join("project")).unwrap(), b"original");
    for (index, path) in candidates.iter().enumerate() {
        assert_eq!(stdfs::read(path).unwrap(), [u8::try_from(index).unwrap()]);
    }
    assert_eq!(
        stdfs::read_dir(&scratch.0).unwrap().count(),
        usize::from(MAX_CONFLICT_ATTEMPTS) + 1
    );
    root.begin_directory(empty_manifest())
        .unwrap()
        .cancel()
        .unwrap();
    assert_eq!(
        stdfs::read_dir(&scratch.0).unwrap().count(),
        usize::from(MAX_CONFLICT_ATTEMPTS) + 1
    );
}

#[test]
fn exhausted_publication_with_failed_cleanup_keeps_quota_and_unknown_entries() {
    let scratch = Scratch::new();
    let root = scratch.root();
    let mut pending = root.begin_directory(empty_manifest()).unwrap();
    pending.verify().unwrap();
    let staging = scratch.0.join(&pending.directories[0].name);
    // Another local actor inserts an entry not owned by this transfer. Its
    // contents are never swept; a failed cleanup must not free the reservation.
    stdfs::write(staging.join("not-owned"), b"preserve").unwrap();
    stdfs::write(scratch.0.join("project"), b"original").unwrap();
    for i in 0..MAX_CONFLICT_ATTEMPTS {
        stdfs::write(
            scratch.0.join(conflict_name("project", pending.nonce, i)),
            [i],
        )
        .unwrap();
    }
    assert_eq!(pending.publish_named().unwrap_err(), Error::Conflict);
    assert_eq!(stdfs::read(staging.join("not-owned")).unwrap(), b"preserve");
    assert_eq!(stdfs::read(scratch.0.join("project")).unwrap(), b"original");
    assert!(matches!(
        root.begin_directory(empty_manifest()),
        Err(Error::Quota)
    ));
    assert!(matches!(
        root.clone()
            .with_conflict_policy(ConflictPolicy::Reject)
            .begin_directory(empty_manifest()),
        Err(Error::Quota)
    ));
}
