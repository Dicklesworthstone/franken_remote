use super::{MAX_REUSES, ScanCache};
use crate::{
    rx::{
        MetadataExt,
        fd::{AsFd, OwnedFd},
    },
    sync::{FolderManifest, SyncError, SyncLimits, source},
};
use std::{
    fs::{self, File, FileTimes},
    io::Write,
    os::unix::fs::{DirBuilderExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, UNIX_EPOCH},
};

struct Directory {
    path: PathBuf,
    root: OwnedFd,
}

impl Directory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-sync-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let root = source::open_root(&path).unwrap();
        Self { path, root }
    }

    fn write(&self, name: &str, bytes: &[u8]) {
        let mut file = File::create(self.path.join(name)).unwrap();
        file.write_all(bytes).unwrap();
        // Stable mtime makes the in-place-edit regression depend on ctime too.
        file.set_times(
            FileTimes::new().set_modified(UNIX_EPOCH + Duration::new(1_700_000_000, 123_456_789)),
        )
        .unwrap();
    }

    fn precise_timestamps(&self, name: &str) -> bool {
        let metadata = fs::metadata(self.path.join(name)).unwrap();
        (1..1_000_000_000).contains(&metadata.mtime_nsec())
            && (1..1_000_000_000).contains(&metadata.ctime_nsec())
    }

    fn canonical(&self, revision: u64, limits: SyncLimits) -> FolderManifest {
        source::scan(self.root.as_fd(), revision, limits, &mut || Ok(())).unwrap()
    }

    fn cached(&self, cache: &mut ScanCache, revision: u64, limits: SyncLimits) -> FolderManifest {
        cache
            .scan(self.root.as_fd(), revision, limits, &mut || Ok(()))
            .unwrap()
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

fn reuses(cache: &ScanCache) -> u8 {
    cache.snapshot.as_ref().unwrap().reuses
}

#[test]
fn unchanged_snapshots_skip_content_work_but_force_a_full_scan_after_three_hits() {
    let directory = Directory::new();
    let limits = SyncLimits {
        max_chunk_bytes: 512,
        ..SyncLimits::default()
    };
    let contents = vec![0x5a; limits.max_chunk_bytes * 128];
    directory.write("document", &contents);
    let mut cache = ScanCache::default();
    let mut full_checks = 0usize;
    let first = cache
        .scan(directory.root.as_fd(), 7, limits, &mut || {
            full_checks += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(reuses(&cache), 0);
    if !directory.precise_timestamps("document") {
        eprintln!("Coarse filesystem timestamps: verifying conservative full-scan fallback.");
        for revision in 8..=12 {
            let current = directory.cached(&mut cache, revision, limits);
            assert!(current == directory.canonical(revision, limits));
            assert_eq!(reuses(&cache), 0);
        }
        return;
    }
    let mut hit_checks = 0usize;
    for expected in 1..=MAX_REUSES {
        hit_checks = 0;
        // An unchanged protocol revision cannot indefinitely refresh cache age.
        let current = cache
            .scan(directory.root.as_fd(), 7, limits, &mut || {
                hit_checks += 1;
                Ok(())
            })
            .unwrap();
        assert!(current == first);
        assert_eq!(reuses(&cache), expected);
        // The real full scanner checks authority around each bounded read.
        // Hits still perform metadata/authority checks, but skip those reads.
        assert!(hit_checks > 0 && full_checks > hit_checks * 4);
    }
    let mut periodic_checks = 0usize;
    let refreshed = cache
        .scan(directory.root.as_fd(), 8, limits, &mut || {
            periodic_checks += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(reuses(&cache), 0);
    assert!(periodic_checks > hit_checks * 4);
    assert!(refreshed == directory.canonical(8, limits));
}

#[test]
fn same_size_edit_with_restored_mtime_is_detected_without_skipping_other_entries() {
    let directory = Directory::new();
    directory.write("a", b"original");
    directory.write("b", b"retained");
    let limits = SyncLimits::default();
    let mut cache = ScanCache::default();
    let first = directory.cached(&mut cache, 1, limits);
    directory.write("a", b"modified");
    let metadata_changed = source::select(directory.root.as_fd(), &first.entries["a"]).is_err();
    let mut revision = 2;
    let mut changed = directory.cached(&mut cache, revision, limits);
    if metadata_changed {
        assert_ne!(
            first.entries["a"].sha256_hex,
            changed.entries["a"].sha256_hex
        );
        assert_eq!(reuses(&cache), 0);
    }
    // Some filesystems can preserve the entire metadata signature across an
    // edit. They must still discover it by the next mandatory full read.
    for _ in 0..MAX_REUSES {
        if first.entries["a"].sha256_hex != changed.entries["a"].sha256_hex {
            break;
        }
        revision += 1;
        changed = directory.cached(&mut cache, revision, limits);
    }
    assert_ne!(
        first.entries["a"].sha256_hex,
        changed.entries["a"].sha256_hex
    );
    assert_eq!(changed.entries.len(), 2);
    assert!(changed == directory.canonical(revision, limits));
    assert_eq!(reuses(&cache), 0);
    // Cache probing and fallback enumeration must not share an exhausted offset.
    assert_eq!(
        changed.entries["b"].sha256_hex,
        first.entries["b"].sha256_hex
    );
}

#[test]
fn replacement_inode_cannot_reuse_an_original_file_digest() {
    let directory = Directory::new();
    directory.write("document", b"original");
    let limits = SyncLimits::default();
    let mut cache = ScanCache::default();
    let first = directory.cached(&mut cache, 1, limits);
    let held = source::select(directory.root.as_fd(), &first.entries["document"]).unwrap();
    directory.write("replacement", b"modified");
    fs::rename(
        directory.path.join("replacement"),
        directory.path.join("document"),
    )
    .unwrap();
    let replaced = directory.cached(&mut cache, 2, limits);
    assert_ne!(
        first.entries["document"].sha256_hex,
        replaced.entries["document"].sha256_hex
    );
    assert!(replaced == directory.canonical(2, limits));
    assert_eq!(reuses(&cache), 0);
    drop(held);
}

#[test]
fn changed_names_replace_the_snapshot_instead_of_accumulating_old_entries() {
    let directory = Directory::new();
    directory.write("before", b"contents");
    let limits = SyncLimits::default();
    let mut cache = ScanCache::default();
    directory.cached(&mut cache, 1, limits);
    fs::rename(directory.path.join("before"), directory.path.join("after")).unwrap();
    let renamed = directory.cached(&mut cache, 2, limits);
    assert!(renamed == directory.canonical(2, limits));
    let snapshot = &cache.snapshot.as_ref().unwrap().manifest;
    assert_eq!(snapshot.entries.len(), 1);
    assert!(!snapshot.entries.contains_key("before"));
    assert!(snapshot.entries.contains_key("after"));
    directory.write("added", b"another");
    let added = directory.cached(&mut cache, 3, limits);
    assert!(added == directory.canonical(3, limits));
    assert_eq!(cache.snapshot.as_ref().unwrap().manifest.entries.len(), 2);
}

#[test]
fn a_different_pinned_root_never_inherits_a_snapshot() {
    let first = Directory::new();
    let second = Directory::new();
    first.write("document", b"original");
    second.write("document", b"modified");
    let limits = SyncLimits::default();
    let mut cache = ScanCache::default();
    first.cached(&mut cache, 1, limits);
    let switched = second.cached(&mut cache, 2, limits);
    assert!(switched == second.canonical(2, limits));
    assert_eq!(reuses(&cache), 0);
}

#[test]
fn narrower_limits_are_rechecked_before_cached_content_is_returned() {
    let directory = Directory::new();
    directory.write("document", b"eight888");
    let mut cache = ScanCache::default();
    directory.cached(&mut cache, 1, SyncLimits::default());
    let limits = SyncLimits {
        max_file_bytes: 4,
        max_total_bytes: 4,
        ..SyncLimits::default()
    };
    assert!(matches!(
        cache.scan(directory.root.as_fd(), 2, limits, &mut || Ok(())),
        Err(SyncError::FileSizeLimitExceeded)
    ));
    assert!(cache.snapshot.is_none());
}

#[test]
fn private_staging_entries_still_exhaust_the_current_enumeration_bound() {
    let directory = Directory::new();
    directory.write("document", b"contents");
    let limits = SyncLimits {
        max_entries: 1,
        ..SyncLimits::default()
    };
    let mut cache = ScanCache::default();
    directory.cached(&mut cache, 1, limits);
    directory.write(&format!("{}cache-test", crate::sync::STAGING_PREFIX), b"");
    assert!(matches!(
        cache.scan(directory.root.as_fd(), 2, limits, &mut || Ok(())),
        Err(SyncError::EntryLimitExceeded)
    ));
    assert!(cache.snapshot.is_none());
}

#[test]
fn empty_or_coarse_timestamp_sources_keep_the_full_content_and_eof_checks() {
    for empty in [false, true] {
        let directory = Directory::new();
        directory.write("document", if empty { b"" } else { b"contents" });
        if !empty {
            File::open(directory.path.join("document"))
                .unwrap()
                .set_times(
                    FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
                )
                .unwrap();
        }
        let limits = SyncLimits::default();
        let mut cache = ScanCache::default();
        for revision in 1..=5 {
            let current = directory.cached(&mut cache, revision, limits);
            assert!(current == directory.canonical(revision, limits));
            assert_eq!(reuses(&cache), 0);
        }
    }
}

#[test]
fn late_authority_loss_discards_the_snapshot_without_returning_cached_data() {
    let directory = Directory::new();
    directory.write("document", b"contents");
    let limits = SyncLimits::default();
    let mut cache = ScanCache::default();
    directory.cached(&mut cache, 1, limits);
    let mut hit_checks = 0usize;
    cache
        .scan(directory.root.as_fd(), 2, limits, &mut || {
            hit_checks += 1;
            Ok(())
        })
        .unwrap();
    assert_eq!(
        reuses(&cache),
        u8::from(directory.precise_timestamps("document"))
    );
    assert!(hit_checks > 3);
    let mut checks = 0usize;
    let result = cache.scan(directory.root.as_fd(), 3, limits, &mut || {
        checks += 1;
        if checks == hit_checks - 1 {
            Err(SyncError::AuthorityRevoked)
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(SyncError::AuthorityRevoked)));
    assert!(cache.snapshot.is_none());
    assert!(directory.cached(&mut cache, 4, limits) == directory.canonical(4, limits));
    assert_eq!(reuses(&cache), 0);
}

#[test]
fn an_edit_during_the_authority_callback_cannot_use_the_prior_digest() {
    let directory = Directory::new();
    directory.write("document", b"original");
    let limits = SyncLimits::default();
    let mut cache = ScanCache::default();
    let first = directory.cached(&mut cache, 1, limits);
    let mut hit_checks = 0usize;
    cache
        .scan(directory.root.as_fd(), 2, limits, &mut || {
            hit_checks += 1;
            Ok(())
        })
        .unwrap();
    if !directory.precise_timestamps("document") {
        eprintln!("Coarse filesystem timestamps: cached callback path is ineligible.");
        assert_eq!(reuses(&cache), 0);
        directory.write("document", b"modified");
        let current = directory.cached(&mut cache, 3, limits);
        assert!(current == directory.canonical(3, limits));
        assert_ne!(
            first.entries["document"].sha256_hex,
            current.entries["document"].sha256_hex
        );
        return;
    }
    assert_eq!(reuses(&cache), 1);
    assert!(hit_checks > 3);
    let mut checks = 0usize;
    let mut edited = false;
    let current = cache
        .scan(directory.root.as_fd(), 3, limits, &mut || {
            checks += 1;
            // Three checks remain after the callback between the checked opens.
            if checks == hit_checks - 3 {
                directory.write("document", b"modified");
                File::open(directory.path.join("document"))
                    .unwrap()
                    .set_times(
                        FileTimes::new()
                            .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_001)),
                    )
                    .unwrap();
                edited = true;
            }
            Ok(())
        })
        .unwrap();
    assert!(edited);
    assert_ne!(
        first.entries["document"].sha256_hex,
        current.entries["document"].sha256_hex
    );
    assert!(current == directory.canonical(3, limits));
    assert_eq!(reuses(&cache), 0);
}

#[test]
fn an_unsafe_replacement_fails_the_canonical_scan_and_clears_old_custody() {
    let directory = Directory::new();
    let other = Directory::new();
    directory.write("document", b"original");
    other.write("target", b"external");
    let limits = SyncLimits::default();
    let mut cache = ScanCache::default();
    directory.cached(&mut cache, 1, limits);
    fs::rename(directory.path.join("document"), other.path.join("retained")).unwrap();
    symlink(other.path.join("target"), directory.path.join("document")).unwrap();
    assert!(matches!(
        cache.scan(directory.root.as_fd(), 2, limits, &mut || Ok(())),
        Err(SyncError::SymlinkForbidden)
    ));
    assert!(cache.snapshot.is_none());
}
