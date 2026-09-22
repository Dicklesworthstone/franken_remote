#![cfg(target_os = "linux")]
use asupersync::atp::object::ContentId;
use fr_files::receive::{DropDirectory, Error, Limits, Publication};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
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
    fn open_with_limits(&self, limits: Limits) -> DropDirectory {
        DropDirectory::open(&self.0, limits).unwrap()
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

use fr_files::receive::{ConflictPolicy, PendingFile};
use std::sync::{Arc, Barrier};

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
fn keep_both_is_explicit_and_reports_the_actual_name_without_logging_it() {
    let scratch = Scratch::new();
    let root = scratch.open();
    fs::write(scratch.0.join("private-report.txt"), b"existing").unwrap();
    assert_eq!(
        verified(&root, "private-report.txt", b"new").publish(),
        Err(Error::Conflict)
    );
    assert_eq!(scratch.entries(), 1);
    let root = root.with_conflict_policy(ConflictPolicy::KeepBoth);
    let receipt = verified(&root, "private-report.txt", b"new")
        .publish_named()
        .unwrap();
    assert!(receipt.was_renamed());
    assert_eq!(receipt.publication(), Publication::Durable);
    assert!(receipt.name().starts_with("private-report.fr-conflict-"));
    assert_eq!(
        std::path::Path::new(receipt.name()).extension().unwrap(),
        "txt"
    );
    assert!(!format!("{receipt:?}").contains("private-report"));
    assert_eq!(
        fs::read(scratch.0.join("private-report.txt")).unwrap(),
        b"existing"
    );
    assert_eq!(fs::read(scratch.0.join(receipt.name())).unwrap(), b"new");
    assert_eq!(scratch.entries(), 2);
    let plain = verified(&root, "unused", b"").publish_named().unwrap();
    assert_eq!(plain.name(), "unused");
    assert!(!plain.was_renamed());
}
#[test]
fn directory_symlink_and_hardlink_conflicts_are_preserved_not_followed() {
    for kind in ["directory", "symlink", "hardlink"] {
        let scratch = Scratch::new();
        let root = scratch
            .open()
            .with_conflict_policy(ConflictPolicy::KeepBoth);
        fs::write(scratch.0.join("original"), b"keep original").unwrap();
        let path = scratch.0.join("occupied");
        match kind {
            "directory" => fs::create_dir(&path).unwrap(),
            "symlink" => symlink(scratch.0.join("original"), &path).unwrap(),
            _ => fs::hard_link(scratch.0.join("original"), &path).unwrap(),
        }
        let receipt = verified(&root, "occupied", b"new file")
            .publish_named()
            .unwrap();
        assert!(receipt.was_renamed());
        assert_eq!(
            fs::read(scratch.0.join(receipt.name())).unwrap(),
            b"new file"
        );
        assert_eq!(
            fs::read(scratch.0.join("original")).unwrap(),
            b"keep original"
        );
        let meta = fs::symlink_metadata(&path).unwrap();
        if kind == "directory" {
            assert!(meta.is_dir());
        }
        if kind == "symlink" {
            assert!(meta.file_type().is_symlink());
        }
    }
}
#[test]
fn keep_both_clones_share_the_original_reservation_budget() {
    let scratch = Scratch::new();
    let root = scratch.open_with_limits(Limits {
        max_file_bytes: 3,
        max_reserved_bytes: 3,
        max_transfers: 1,
    });
    let alternate = root.clone().with_conflict_policy(ConflictPolicy::KeepBoth);
    let first = verified(&root, "first", b"abc");
    assert_eq!(
        alternate
            .begin("second", 1, ContentId::from_bytes(b"x"))
            .unwrap_err(),
        Error::Quota
    );
    first.cancel().unwrap();
    assert_eq!(
        verified(&alternate, "second", b"x").publish(),
        Ok(Publication::Durable)
    );
}
#[test]
fn conflict_after_verification_uses_pinned_directory_even_if_its_path_is_replaced() {
    let scratch = Scratch::new();
    let original = scratch.0.join("selected");
    fs::create_dir(&original).unwrap();
    fs::set_permissions(&original, fs::Permissions::from_mode(0o700)).unwrap();
    let root = DropDirectory::open(
        &original,
        Limits {
            max_file_bytes: 1024,
            max_reserved_bytes: 1024,
            max_transfers: 2,
        },
    )
    .unwrap()
    .with_conflict_policy(ConflictPolicy::KeepBoth);
    let pending = verified(&root, "report", b"incoming");
    fs::write(original.join("report"), b"arrived concurrently").unwrap();
    let retained = scratch.0.join("retained");
    fs::rename(&original, &retained).unwrap();
    fs::create_dir(&original).unwrap();
    let receipt = pending.publish_named().unwrap();
    assert!(receipt.was_renamed());
    assert_eq!(fs::read_dir(&original).unwrap().count(), 0);
    assert_eq!(
        fs::read(retained.join("report")).unwrap(),
        b"arrived concurrently"
    );
    assert_eq!(
        fs::read(retained.join(receipt.name())).unwrap(),
        b"incoming"
    );
}
#[test]
fn concurrent_same_name_publications_keep_every_verified_file() {
    let scratch = Scratch::new();
    let root = scratch
        .open_with_limits(Limits {
            max_file_bytes: 1024,
            max_reserved_bytes: 8192,
            max_transfers: 8,
        })
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    let gate = Arc::new(Barrier::new(8));
    let threads = (0_u8..8)
        .map(|value| {
            let root = root.clone();
            let gate = gate.clone();
            std::thread::spawn(move || {
                let pending = verified(&root, "shared.bin", &[value]);
                gate.wait();
                (value, pending.publish_named().unwrap())
            })
        })
        .collect::<Vec<_>>();
    let mut names = std::collections::BTreeSet::new();
    for thread in threads {
        let (value, receipt) = thread.join().unwrap();
        assert!(names.insert(receipt.name().to_owned()));
        assert_eq!(fs::read(scratch.0.join(receipt.name())).unwrap(), [value]);
    }
    assert!(names.contains("shared.bin"));
    assert_eq!(scratch.entries(), 8);
}
#[test]
fn exhausted_conflict_candidates_fail_bounded_without_changing_existing_entries() {
    let scratch = Scratch::new();
    let root = scratch
        .open()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    let pending = verified(&root, "report.txt", b"incoming");
    let staging = fs::read_dir(&scratch.0)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    let staging = staging.to_str().unwrap();
    let nonce = staging.strip_prefix(".fr-part-").unwrap();
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
