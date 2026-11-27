//! Actual filesystem snapshots share the production sender's traversal and ATP
//! manifest calculation. No transport or publication is inferred from scanning.
use super::tests::{event, fixture, prepared, reap, selected};
use super::*;
use std::fs;

#[test]
fn subtree_snapshot_matches_independent_sender_manifest_and_descendant_count() {
    fixture(|path, cx| {
        fs::create_dir_all(path.join("nested/empty")).unwrap();
        fs::write(path.join("nested/data"), b"payload").unwrap();
        fs::write(path.join("top"), b"data").unwrap();
        let mut checks = 0;
        let snapshot = snapshot_selected(
            File::open(path).unwrap(),
            "selected",
            true,
            11,
            4,
            &mut || {
                checks += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(snapshot.inspected_entries, 4);
        assert_eq!(snapshot.version.size(), 11);
        assert!(checks > 4, "the original scan callback remains active");
        let mut source = selected(path, cx);
        let manifest = prepared(&mut source);
        assert!(snapshot.version.matches(&manifest));
        assert!(matches!(
            &snapshot.version,
            ExpectedSource::Tree {
                metadata_root: Some(_),
                ..
            }
        ));
        source.stop();
        reap(&mut source);
    });
}

#[test]
fn subtree_snapshot_cannot_exceed_remaining_global_nodes_or_bytes() {
    fixture(|path, _cx| {
        let empty = snapshot_selected(
            File::open(path).unwrap(),
            "selected",
            true,
            0,
            0,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(empty.inspected_entries, 0);
        assert_eq!(empty.version.size(), 0);
        assert!(matches!(empty.version, ExpectedSource::Tree { .. }));
        fs::create_dir_all(path.join("nested/empty")).unwrap();
        fs::write(path.join("nested/data"), b"payload").unwrap();
        fs::write(path.join("top"), b"data").unwrap();
        for (bytes, nodes) in [(11, 3), (10, 4), (11, 0)] {
            assert!(matches!(
                snapshot_selected(
                    File::open(path).unwrap(),
                    "selected",
                    true,
                    bytes,
                    nodes,
                    &mut || Ok(())
                ),
                Err(Error::Limits)
            ));
        }
    });
}

#[test]
fn snapshot_cancellation_keeps_the_original_stop_and_admits_no_success() {
    fixture(|path, _cx| {
        fs::create_dir_all(path.join("nested/empty")).unwrap();
        fs::write(path.join("nested/data"), b"payload").unwrap();
        let mut calls = 0;
        assert!(matches!(
            snapshot_selected(
                File::open(path).unwrap(),
                "selected",
                true,
                100,
                8,
                &mut || {
                    calls += 1;
                    if calls >= 3 {
                        Err(Error::Cancelled)
                    } else {
                        Ok(())
                    }
                },
            ),
            Err(Error::Cancelled)
        ));
        assert_eq!(calls, 3, "cancellation cannot restart another walk");
        assert!(matches!(
            snapshot_selected(
                File::open(path).unwrap(),
                "selected",
                true,
                100,
                8,
                &mut || Err(Error::Expired),
            ),
            Err(Error::Expired)
        ));
    });
}

#[test]
fn regular_snapshot_uses_the_exact_sha_and_no_descendant_budget() {
    fixture(|path, cx| {
        let pathname = path.join("regular");
        fs::write(&pathname, b"payload").unwrap();
        let snapshot = snapshot_selected(
            File::open(&pathname).unwrap(),
            "selected",
            false,
            7,
            0,
            &mut || Ok(()),
        )
        .unwrap();
        assert_eq!(snapshot.inspected_entries, 0);
        assert_eq!(snapshot.version.size(), 7);
        let mut source = Source::spawn(
            cx.clone(),
            File::open(&pathname).unwrap(),
            "selected".into(),
            Policy::default(),
            cx.timer_driver().unwrap().now().as_nanos() / 1000 + 2_000_000,
        )
        .unwrap();
        let manifest = prepared(&mut source);
        assert!(snapshot.version.matches(&manifest));
        let ExpectedSource::File { sha256, .. } = &snapshot.version else {
            panic!("a regular file was classified as a tree");
        };
        assert_eq!(sha256, &manifest.entries[0].sha256_hex);
        assert!(!format!("{:?}", snapshot.version).contains(sha256));
        source.stop();
        reap(&mut source);
    });
}

#[test]
fn empty_file_and_empty_directory_leaf_are_distinct_source_versions() {
    fixture(|path, _cx| {
        let directory_tree = path.join("directory-tree");
        let file_tree = path.join("file-tree");
        fs::create_dir_all(directory_tree.join("leaf")).unwrap();
        fs::create_dir(&file_tree).unwrap();
        fs::write(file_tree.join("leaf"), b"").unwrap();
        let scan = |root| {
            snapshot_selected(
                File::open(root).unwrap(),
                "selected",
                true,
                1,
                1,
                &mut || Ok(()),
            )
            .unwrap()
        };
        let directory = scan(&directory_tree);
        let regular = scan(&file_tree);
        assert_eq!(directory.version.size(), 0);
        assert_eq!(regular.version.size(), 0);
        assert_ne!(directory.version, regular.version);
        assert_ne!(directory.version.ledger_key(), regular.version.ledger_key());
        assert!(matches!(
            directory.version,
            ExpectedSource::Tree {
                metadata_root: Some(_),
                ..
            }
        ));
        assert!(matches!(
            regular.version,
            ExpectedSource::Tree {
                metadata_root: None,
                ..
            }
        ));
    });
}

#[test]
fn subtree_snapshot_refuses_links_and_unsafe_names() {
    fixture(|path, _cx| {
        std::os::unix::fs::symlink("/unread-target", path.join("link")).unwrap();
        assert!(matches!(
            snapshot_selected(
                File::open(path).unwrap(),
                "selected",
                true,
                100,
                8,
                &mut || Ok(()),
            ),
            Err(Error::Source)
        ));
        assert!(matches!(
            snapshot_selected(
                File::open(path).unwrap(),
                "../selected",
                true,
                100,
                8,
                &mut || Ok(()),
            ),
            Err(Error::Name)
        ));
    });
}

#[test]
fn an_expected_version_cannot_hash_growth_under_the_ordinary_larger_limit() {
    for directory in [false, true] {
        for original in [b"".as_slice(), b"data".as_slice()] {
            fixture(|path, cx| {
                let content = path.join("payload");
                fs::write(&content, original).unwrap();
                let root = if directory { path } else { &content };
                let snapshot = snapshot_selected(
                    File::open(root).unwrap(),
                    "selected",
                    directory,
                    100,
                    1,
                    &mut || Ok(()),
                )
                .unwrap();
                fs::write(&content, b"grown beyond the scanned version").unwrap();
                let mut source = Source::spawn_selected(
                    cx.clone(),
                    SelectedSource {
                        file: File::open(root).unwrap(),
                        name: "selected".into(),
                        directory,
                        expected_bytes: Some(snapshot.version.size()),
                    },
                    Policy::default(),
                    cx.timer_driver().unwrap().now().as_nanos() / 1000 + 2_000_000,
                    None,
                )
                .unwrap();
                assert!(matches!(event(&mut source), Err(Error::SourceChanged)));
                reap(&mut source);
            });
        }
    }
}

#[test]
fn truncation_during_snapshot_hashing_is_a_retryable_source_change() {
    for directory in [false, true] {
        fixture(|path, _cx| {
            let pathname = path.join("payload");
            fs::write(&pathname, vec![7_u8; 2 * MAX_CHUNK_BYTES]).unwrap();
            let selected_path = if directory { path } else { &pathname };
            let mut selected = tree::Selection::new(
                File::open(selected_path).unwrap(),
                "selected",
                directory,
                (2 * MAX_CHUNK_BYTES) as u64,
                &mut || Ok(()),
            )
            .unwrap();
            let tree::Contents::File { file, .. } = &selected.entries[0].contents else {
                panic!("the selected payload must be a regular file");
            };
            // This clone shares the original file offset, so the edit happens
            // only after a real bounded content read, independent of the number
            // of authority checks or directory-enumeration callbacks.
            let mut position = file.try_clone().unwrap();
            let editor = fs::OpenOptions::new().write(true).open(&pathname).unwrap();
            let mut truncated = false;
            let result = prepare(&mut selected, "selected".into(), directory, &mut || {
                if !truncated && position.stream_position().unwrap() >= MAX_CHUNK_BYTES as u64 {
                    editor.set_len(MAX_CHUNK_BYTES as u64).unwrap();
                    truncated = true;
                }
                Ok(())
            });
            assert!(truncated, "the source changed after its first content read");
            assert!(matches!(result, Err(Error::SourceChanged)));

            // A fresh scan can inspect the settled shorter version. No
            // partially hashed manifest escaped the failed original scan.
            let fresh = snapshot_selected(
                File::open(selected_path).unwrap(),
                "selected",
                directory,
                (2 * MAX_CHUNK_BYTES) as u64,
                1,
                &mut || Ok(()),
            )
            .unwrap();
            assert_eq!(fresh.version.size(), MAX_CHUNK_BYTES as u64);
        });
    }
}
