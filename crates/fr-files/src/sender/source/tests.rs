//! Real descriptor walk/hash/read owner; no transport or admission is simulated
//! by these tests. The native QUIC sender suite exercises the composed lane.
use super::*;
use asupersync::runtime::RuntimeBuilder;
use std::{fs, os::unix::fs::DirBuilderExt, path::Path, sync::atomic::AtomicU64, time::Instant};

static NEXT: AtomicU64 = AtomicU64::new(0);
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fr-directory-source-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn fixture(test: impl FnOnce(&Path, &Cx)) {
    RuntimeBuilder::new()
        .worker_threads(2)
        .enable_platform_reactor(true)
        .build()
        .unwrap()
        .block_on(async {
            let cx = Cx::current().unwrap();
            let scratch = Scratch::new();
            test(&scratch.0, &cx);
        });
}
fn selected(path: &Path, cx: &Cx) -> Source {
    Source::spawn_directory(
        cx.clone(),
        File::open(path).unwrap(),
        "selected".into(),
        Policy::default(),
        cx.timer_driver().unwrap().now().as_nanos() / 1000 + 2_000_000,
    )
    .unwrap()
}
fn event(source: &mut Source) -> Result<Event, Error> {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(event) = source.poll()? {
            return Ok(event);
        }
        assert!(Instant::now() < until, "source did not report its result");
        thread::yield_now();
    }
}
fn prepared(source: &mut Source) -> TransferManifest {
    let Event::Prepared(manifest) = event(source).unwrap() else {
        panic!("source did not prepare its original selection");
    };
    *manifest
}
fn reap(source: &mut Source) {
    let until = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(result) = source.reap() {
            result.unwrap();
            return;
        }
        assert!(Instant::now() < until, "source did not finish cleanup");
        thread::yield_now();
    }
}
fn directory_metadata() -> EntryMetadata {
    EntryMetadata {
        file_kind: FileKind::Directory,
        ..EntryMetadata::default()
    }
}

#[test]
fn structural_entries_commit_regular_files_too_and_never_produce_chunks() {
    fixture(|path, cx| {
        fs::create_dir(path.join("a-empty")).unwrap();
        fs::write(path.join("b-zero"), b"").unwrap();
        fs::write(path.join("c-payload"), b"payload").unwrap();
        fs::create_dir_all(path.join("deep/leaf")).unwrap();
        let mut source = selected(path, cx);
        let manifest = prepared(&mut source);
        assert!(manifest.is_directory);
        assert_eq!(manifest.total_bytes, 7);
        assert_eq!(
            manifest
                .entries
                .iter()
                .map(|entry| (entry.index, entry.rel_path.as_str(), entry.size))
                .collect::<Vec<_>>(),
            [
                (0, "a-empty", 0),
                (1, "b-zero", 0),
                (2, "c-payload", 7),
                (3, "deep/leaf", 0),
            ]
        );
        for index in [0, 3] {
            let entry = &manifest.entries[index];
            assert_eq!(entry.metadata.as_ref(), Some(&directory_metadata()));
            assert_eq!(entry.sha256_hex, EMPTY_SHA256);
            assert!(entry.members.is_empty());
        }
        assert!(manifest.entries[1].metadata.is_none());
        assert_eq!(manifest.entries[1].sha256_hex, EMPTY_SHA256);
        assert!(manifest.entries[2].metadata.is_none());
        // Independently encoded pinned ATP v1 fixture: all four sorted paths,
        // kind bytes 2/0/0/2, absent optional metadata and empty xattr maps.
        assert_eq!(
            manifest.metadata_root_hex.as_deref(),
            Some("9c9f2ddc7b432c3613932249d72c19d2181aac34e3faffc8d2006b4661286d50")
        );
        assert!(manifest.directory_metadata.is_none());
        assert!(manifest.delta_manifest.is_none());
        let mut received = Vec::new();
        loop {
            source.request(3).unwrap();
            match event(&mut source).unwrap() {
                Event::Chunk {
                    index,
                    offset,
                    bytes,
                } => {
                    assert_eq!(index, 2);
                    assert_eq!(offset, received.len() as u64);
                    assert!(!bytes.is_empty() && bytes.len() <= 3);
                    received.extend(bytes);
                }
                Event::End => break,
                Event::Prepared(_) => panic!("selection was restarted"),
            }
        }
        assert_eq!(received, b"payload");
        reap(&mut source);
    });
}

#[test]
fn empty_only_tree_completes_without_reading_any_directory_as_a_file() {
    fixture(|path, cx| {
        fs::create_dir_all(path.join("first/leaf")).unwrap();
        fs::create_dir(path.join("second")).unwrap();
        let mut source = selected(path, cx);
        let manifest = prepared(&mut source);
        assert_eq!(manifest.total_bytes, 0);
        assert_eq!(manifest.entries.len(), 2);
        assert!(manifest.metadata_root_hex.is_some());
        assert!(manifest.entries.iter().all(|entry| {
            entry.size == 0
                && entry.sha256_hex == EMPTY_SHA256
                && entry.metadata.as_ref() == Some(&directory_metadata())
        }));
        source.request(1).unwrap();
        assert!(matches!(event(&mut source), Ok(Event::End)));
        reap(&mut source);
    });
}

#[test]
fn maximum_empty_leaf_entries_are_not_double_counted_as_nodes() {
    for nested in [false, true] {
        fixture(|path, cx| {
            for index in 0..crate::receive::directory::MAX_TREE_ENTRIES {
                let leaf = if nested {
                    format!("branch-{index:02}/empty")
                } else {
                    format!("empty-{index:02}")
                };
                fs::create_dir_all(path.join(leaf)).unwrap();
            }
            // The nested case reaches exactly 128 non-root nodes as well as
            // exactly 64 manifest entries; retained directory fds count once.
            let mut source = selected(path, cx);
            let manifest = prepared(&mut source);
            assert_eq!(
                manifest.entries.len(),
                crate::receive::directory::MAX_TREE_ENTRIES
            );
            assert_eq!(manifest.total_bytes, 0);
            assert!(
                manifest
                    .entries
                    .iter()
                    .all(|entry| { entry.metadata.as_ref() == Some(&directory_metadata()) })
            );
            assert!(
                serde_json::to_vec(&manifest).unwrap().len()
                    <= crate::atp::receive::MAX_MANIFEST_BYTES
            );
            source.request(1).unwrap();
            assert!(matches!(event(&mut source), Ok(Event::End)));
            reap(&mut source);
        });
    }
}

#[test]
fn empty_root_and_file_only_trees_keep_legacy_manifests_without_metadata() {
    fixture(|path, cx| {
        for content in [false, true] {
            if content {
                fs::create_dir(path.join("nested")).unwrap();
                fs::write(path.join("nested/empty-file"), b"").unwrap();
            }
            let mut source = selected(path, cx);
            let manifest = prepared(&mut source);
            assert_eq!(manifest.entries.len(), usize::from(content));
            assert!(
                manifest
                    .entries
                    .iter()
                    .all(|entry| entry.metadata.is_none())
            );
            assert!(manifest.metadata_root_hex.is_none());
            assert!(manifest.directory_metadata.is_none());
            let encoded = serde_json::to_value(&manifest).unwrap();
            assert!(encoded.get("metadata_root_hex").is_none());
            assert!(encoded.get("directory_metadata").is_none());
            source.request(1).unwrap();
            assert!(matches!(event(&mut source), Ok(Event::End)));
            reap(&mut source);
        }
    });
}

#[test]
fn empty_leaf_mutation_after_selection_is_refused_before_manifest() {
    fixture(|path, cx| {
        fs::create_dir_all(path.join("nested/empty")).unwrap();
        let clock = cx.timer_driver().unwrap();
        let gate = Gate {
            stop: Arc::new(AtomicBool::new(false)),
            cx: cx.clone(),
            deadline: clock.now().as_nanos() / 1000 + 2_000_000,
            clock,
        };
        let mut tree =
            tree::Selection::new(File::open(path).unwrap(), "selected", true, 100, &gate).unwrap();
        fs::write(path.join("nested/empty/added"), b"late").unwrap();
        assert!(matches!(
            prepare(&mut tree, "selected".into(), true, &gate),
            Err(Error::SourceChanged)
        ));
    });
}

#[test]
fn changes_to_empty_leaves_after_manifest_are_refused_before_completion() {
    for replace in [false, true] {
        fixture(|path, cx| {
            fs::create_dir_all(path.join("nested/empty")).unwrap();
            let mut source = selected(path, cx);
            let manifest = prepared(&mut source);
            assert_eq!(manifest.entries.len(), 1);
            if replace {
                fs::rename(path.join("nested/empty"), path.join("nested/original")).unwrap();
                fs::create_dir(path.join("nested/empty")).unwrap();
            } else {
                fs::write(path.join("nested/empty/added"), b"late").unwrap();
            }
            source.request(1).unwrap();
            assert!(matches!(event(&mut source), Err(Error::SourceChanged)));
            reap(&mut source);
        });
    }
}
