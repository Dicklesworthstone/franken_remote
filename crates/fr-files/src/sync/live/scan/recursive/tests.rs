use super::*;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-recursive-scan-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn descriptor(&self) -> crate::rx::fd::OwnedFd {
        source::open_root(&self.0).unwrap()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
use crate::rx::fd::AsFd;

fn ledger(round: &PreparedRound) -> BTreeMap<String, String> {
    round
        .versions
        .iter()
        .map(|version| (version.name.clone(), version.hash.clone()))
        .collect()
}

#[test]
fn nested_files_and_empty_directories_share_one_atomic_top_level_version() {
    let root = Root::new();
    fs::create_dir_all(root.0.join("project/nested/empty")).unwrap();
    fs::write(root.0.join("project/nested/file"), b"nested contents").unwrap();
    fs::write(root.0.join("project/empty-file"), b"").unwrap();
    fs::write(root.0.join("readme"), b"root file").unwrap();
    let fd = root.descriptor();
    let round = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &BTreeMap::new(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(round.selection.len(), 2);
    assert_eq!(round.versions.len(), 2);
    assert_eq!(round.versions[0].name, "project");
    assert_eq!(round.versions[0].bytes, 15);
    assert_eq!(round.versions[1].name, "readme");
    assert_eq!(round.bytes, 24);
    let unchanged = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &ledger(&round),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert!(unchanged.selection.is_empty());
    assert_eq!(unchanged.bytes, 0);
}

#[test]
fn descendant_edit_is_detected_without_a_parent_timestamp_change() {
    let root = Root::new();
    fs::create_dir_all(root.0.join("project/nested")).unwrap();
    fs::write(root.0.join("project/nested/file"), b"before").unwrap();
    fs::write(root.0.join("unchanged"), b"stable").unwrap();
    let fd = root.descriptor();
    let first = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &BTreeMap::new(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    let before = source::identity(&fs::metadata(root.0.join("project")).unwrap());
    fs::write(root.0.join("project/nested/file"), b"after!").unwrap();
    let after = source::identity(&fs::metadata(root.0.join("project")).unwrap());
    assert!(
        before == after,
        "writing an existing descendant must not change its ancestor"
    );
    let changed = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &ledger(&first),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(changed.selection.len(), 1);
    assert_eq!(changed.versions[0].name, "project");
    assert_ne!(changed.versions[0].hash, first.versions[0].hash);
    assert_eq!(changed.bytes, 6);
}

#[test]
fn empty_file_and_empty_directory_versions_are_distinct() {
    let root = Root::new();
    let retired = Root::new();
    fs::create_dir_all(root.0.join("project")).unwrap();
    fs::write(root.0.join("project/entry"), b"").unwrap();
    let fd = root.descriptor();
    let first = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &BTreeMap::new(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    fs::rename(root.0.join("project/entry"), retired.0.join("old")).unwrap();
    fs::create_dir(root.0.join("project/entry")).unwrap();
    let next = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &ledger(&first),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(next.selection.len(), 1);
    assert_eq!(next.bytes, 0);
    assert_ne!(next.versions[0].hash, first.versions[0].hash);
}

#[test]
fn all_descendants_and_private_root_entries_count_against_one_scan_budget() {
    let root = Root::new();
    for name in ["a", "b"] {
        fs::create_dir(root.0.join(name)).unwrap();
        fs::write(root.0.join(name).join("one"), b"1").unwrap();
        fs::write(root.0.join(name).join("two"), b"2").unwrap();
    }
    fs::write(root.0.join(".fr-part-private"), b"private").unwrap();
    let fd = root.descriptor();
    let mut limits = SyncLimits {
        max_entries: 6,
        ..SyncLimits::default()
    };
    assert!(matches!(
        prepare(fd.as_fd(), limits, &BTreeMap::new(), None, &mut || Ok(())),
        Err(LiveError::Resource)
    ));
    limits.max_entries = 7;
    let round = prepare(fd.as_fd(), limits, &BTreeMap::new(), None, &mut || Ok(())).unwrap();
    assert_eq!(round.selection.len(), 2);
    assert_eq!(round.bytes, 4);
}

#[test]
fn whole_trees_and_aggregate_content_keep_the_existing_byte_ceilings() {
    let root = Root::new();
    fs::create_dir(root.0.join("a")).unwrap();
    fs::create_dir(root.0.join("b")).unwrap();
    fs::write(root.0.join("a/file"), b"1234").unwrap();
    fs::write(root.0.join("b/file"), b"5678").unwrap();
    let fd = root.descriptor();
    let limits = SyncLimits {
        max_file_bytes: 4,
        max_total_bytes: 7,
        ..SyncLimits::default()
    };
    assert!(matches!(
        prepare(fd.as_fd(), limits, &BTreeMap::new(), None, &mut || Ok(())),
        Err(LiveError::Resource)
    ));
    fs::write(root.0.join("a/second"), b"x").unwrap();
    let limits = SyncLimits {
        max_file_bytes: 4,
        max_total_bytes: 8,
        ..SyncLimits::default()
    };
    assert!(prepare(fd.as_fd(), limits, &BTreeMap::new(), None, &mut || Ok(())).is_err());
}

#[test]
fn links_refuse_without_following_their_targets() {
    let root = Root::new();
    let outside = Root::new();
    fs::write(outside.0.join("private"), b"outside").unwrap();
    symlink(&outside.0, root.0.join("alias")).unwrap();
    let fd = root.descriptor();
    assert!(matches!(
        prepare(
            fd.as_fd(),
            SyncLimits::default(),
            &BTreeMap::new(),
            None,
            &mut || Ok(())
        ),
        Err(LiveError::Source(SyncError::SymlinkForbidden))
    ));
    fs::rename(root.0.join("alias"), outside.0.join("retired-link")).unwrap();
    fs::create_dir(root.0.join("project")).unwrap();
    symlink(outside.0.join("private"), root.0.join("project/alias")).unwrap();
    assert!(
        prepare(
            fd.as_fd(),
            SyncLimits::default(),
            &BTreeMap::new(),
            None,
            &mut || Ok(())
        )
        .is_err()
    );
    assert_eq!(fs::read(outside.0.join("private")).unwrap(), b"outside");
}

#[test]
fn cancellation_during_recursive_work_retains_the_original_reason() {
    let root = Root::new();
    fs::create_dir_all(root.0.join("a/b")).unwrap();
    fs::write(root.0.join("a/b/file"), vec![1_u8; 512 * 1024]).unwrap();
    let fd = root.descriptor();
    let mut checks = 0;
    let result = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &BTreeMap::new(),
        None,
        &mut || {
            checks += 1;
            if checks >= 10 {
                Err(SyncError::AuthorityRevoked)
            } else {
                Ok(())
            }
        },
    );
    assert!(matches!(
        result,
        Err(LiveError::Source(SyncError::AuthorityRevoked))
    ));
    assert_eq!(checks, 10);
}

#[test]
fn deferred_tree_versions_use_the_existing_fair_page_cursor() {
    let root = Root::new();
    for index in 0..35 {
        fs::create_dir(root.0.join(format!("tree-{index:02}"))).unwrap();
    }
    let fd = root.descriptor();
    let first = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &BTreeMap::new(),
        None,
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(first.selection.len(), 32);
    let saved = ledger(&first);
    let next = prepare(
        fd.as_fd(),
        SyncLimits::default(),
        &saved,
        Some("tree-31"),
        &mut || Ok(()),
    )
    .unwrap();
    assert_eq!(next.selection.len(), 3);
    assert_eq!(next.versions[0].name, "tree-32");
    assert_eq!(next.bytes, 0);
}
