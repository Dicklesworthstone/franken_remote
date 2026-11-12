use super::*;
use std::{
    fs as disk,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    path::PathBuf,
    time::Instant,
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        // macOS's temporary directory is reached through the /var symlink,
        // which the no-follow walk refuses by design: use the real path.
        let path = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "frd-unit-stage-{:032x}",
            u128::from_ne_bytes(random)
        ));
        disk::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = disk::remove_dir_all(&self.0);
    }
}

#[test]
fn abandoning_an_owned_stage_removes_it_without_touching_the_installed_unit() {
    let root = Root::new();
    let dir = directory(&root.0, false).unwrap().unwrap();
    let existing = root.0.join("frd.service");
    disk::write(&existing, b"original complete service").unwrap();
    let mut stage = Stage::create(&dir).unwrap();
    let pending = root.0.join(&stage.name);
    stage.file.write_all(b"incomplete replacement").unwrap();
    assert!(pending.is_file());

    drop(stage);
    assert!(!pending.exists());
    assert_eq!(disk::read(&existing).unwrap(), b"original complete service");
}

#[test]
fn refusal_after_staging_preserves_the_new_link_and_retires_only_the_owned_stage() {
    let root = Root::new();
    let dir = directory(&root.0, false).unwrap().unwrap();
    let mut stage = Stage::create(&dir).unwrap();
    let pending = root.0.join(&stage.name);
    stage.file.write_all(b"replacement service").unwrap();
    stage.file.sync_all().unwrap();
    let victim = root.0.join("victim");
    disk::write(&victim, b"preserved").unwrap();
    let unit = root.0.join("frd.service");
    symlink(&victim, &unit).unwrap();

    assert_eq!(
        stage.publish(OsStr::new("frd.service")),
        Err(ServiceError::UnsafeUnitPath)
    );
    assert!(!pending.exists());
    assert_eq!(disk::read_link(&unit).unwrap(), victim);
    assert_eq!(disk::read(&victim).unwrap(), b"preserved");
}

#[test]
fn a_substituted_stage_name_is_neither_published_nor_deleted() {
    let root = Root::new();
    let dir = directory(&root.0, false).unwrap().unwrap();
    let mut stage = Stage::create(&dir).unwrap();
    let pending = root.0.join(&stage.name);
    stage.file.write_all(b"owned stage").unwrap();
    stage.file.sync_all().unwrap();
    let moved = root.0.join("retained-original-stage");
    disk::rename(&pending, &moved).unwrap();
    disk::write(&pending, b"a different file now owns this name").unwrap();

    assert_eq!(
        stage.publish(OsStr::new("frd.service")),
        Err(ServiceError::UnsafeUnitPath)
    );
    assert!(!root.0.join("frd.service").exists());
    assert_eq!(disk::read(&moved).unwrap(), b"owned stage");
    assert_eq!(
        disk::read(&pending).unwrap(),
        b"a different file now owns this name"
    );
}

// Reproduce statat's lookup/metadata race with real inodes: retain the original
// descriptor, atomically replace its directory entry, then inspect the now
// unlinked old inode. Only the first snapshot is interposed; every retry reads
// the actual destination through the production no-follow directory handle.
fn replaced_unit(root: &Root, replacement: &Path) -> (OwnedFd, File) {
    let unit = root.0.join("frd.service");
    disk::write(&unit, b"original complete service").unwrap();
    let original = File::open(&unit).unwrap();
    disk::rename(replacement, &unit).unwrap();
    assert_eq!(fs::fstat(&original).unwrap().st_nlink, 0);
    (directory(&root.0, false).unwrap().unwrap(), original)
}

#[test]
fn a_raced_away_unit_requires_a_fresh_safe_destination_snapshot() {
    let root = Root::new();
    let replacement = root.0.join("complete-replacement");
    disk::write(&replacement, b"new complete service").unwrap();
    let (dir, original) = replaced_unit(&root, &replacement);
    let mut reads = 0;
    let result = inspect_destination(|| {
        reads += 1;
        if reads == 1 {
            fs::fstat(&original)
        } else {
            fs::statat(&dir, "frd.service", AtFlags::SYMLINK_NOFOLLOW)
        }
    });

    assert_eq!(result, Ok(true));
    assert_eq!(reads, 2);
    assert_eq!(
        disk::read(root.0.join("frd.service")).unwrap(),
        b"new complete service"
    );
}

#[test]
fn an_unlinked_snapshot_outlasts_a_replacement_still_in_flight() {
    // A descheduled renamer can leave the name on the unlinked inode for
    // milliseconds; here the replacement becomes visible only after 20 ms.
    let root = Root::new();
    let replacement = root.0.join("complete-replacement");
    disk::write(&replacement, b"new complete service").unwrap();
    let (dir, original) = replaced_unit(&root, &replacement);
    let start = Instant::now();
    let mut reads = 0;
    let result = inspect_destination(|| {
        reads += 1;
        if reads == 1 || start.elapsed() < Duration::from_millis(20) {
            fs::fstat(&original)
        } else {
            fs::statat(&dir, "frd.service", AtFlags::SYMLINK_NOFOLLOW)
        }
    });

    assert_eq!(result, Ok(true));
    assert!(reads < DESTINATION_SNAPSHOTS, "{reads} snapshots");
}

#[test]
fn a_raced_away_unit_can_be_absent_but_an_unlinked_snapshot_never_suffices() {
    let root = Root::new();
    let unit = root.0.join("frd.service");
    disk::write(&unit, b"removed complete service").unwrap();
    let original = File::open(&unit).unwrap();
    disk::remove_file(&unit).unwrap();
    let dir = directory(&root.0, false).unwrap().unwrap();
    assert_eq!(fs::fstat(&original).unwrap().st_nlink, 0);
    let mut reads = 0;
    assert_eq!(
        inspect_destination(|| {
            reads += 1;
            if reads == 1 {
                fs::fstat(&original)
            } else {
                fs::statat(&dir, "frd.service", AtFlags::SYMLINK_NOFOLLOW)
            }
        }),
        Ok(false)
    );
    assert_eq!(reads, 2);

    let mut reads = 0;
    assert_eq!(
        inspect_destination(|| {
            reads += 1;
            fs::fstat(&original)
        }),
        Err(ServiceError::UnsafeUnitPath)
    );
    assert_eq!(reads, DESTINATION_SNAPSHOTS);
    assert!(!unit.exists());
}

#[test]
fn revalidation_refuses_an_unsafe_replacement_without_a_further_retry() {
    for kind in ["symlink", "hardlink", "writable"] {
        let root = Root::new();
        let victim = root.0.join("unrelated-file");
        disk::write(&victim, b"preserved unrelated bytes").unwrap();
        let replacement = root.0.join("unsafe-replacement");
        match kind {
            "symlink" => symlink(&victim, &replacement).unwrap(),
            "hardlink" => disk::hard_link(&victim, &replacement).unwrap(),
            "writable" => {
                disk::write(&replacement, b"shared-writable service").unwrap();
                disk::set_permissions(&replacement, disk::Permissions::from_mode(0o666)).unwrap();
            }
            _ => unreachable!(),
        }
        let (dir, original) = replaced_unit(&root, &replacement);
        let mut reads = 0;
        assert_eq!(
            inspect_destination(|| {
                reads += 1;
                if reads == 1 {
                    fs::fstat(&original)
                } else {
                    fs::statat(&dir, "frd.service", AtFlags::SYMLINK_NOFOLLOW)
                }
            }),
            Err(ServiceError::UnsafeUnitPath),
            "{kind}"
        );
        assert_eq!(reads, 2, "{kind}");
        assert_eq!(disk::read(&victim).unwrap(), b"preserved unrelated bytes");
        assert!(disk::symlink_metadata(root.0.join("frd.service")).is_ok());
    }
}
