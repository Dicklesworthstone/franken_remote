//! Structural ATP entries retain their kind through real confined publication.
use super::*;
use asupersync::net::atp::transport_common::{EntryMetadata, FileKind, metadata_commitment};
use fr_files::receive::ConflictPolicy;
use std::os::unix::fs::MetadataExt;

type Item<'a> = (&'a str, Option<&'a [u8]>);

fn parts(items: &[Item<'_>]) -> (Vec<ManifestEntry>, String) {
    let mut entries = Vec::new();
    let mut digests = Vec::new();
    for (index, (path, content)) in items.iter().enumerate() {
        let bytes = content.unwrap_or_default();
        let mut hasher = StagedEntryReceive::new("hash-only".into());
        hasher.update_with_chunk(bytes);
        let (digest, _, _) = hasher.finalize((*path).into());
        entries.push(ManifestEntry {
            index: u32::try_from(index).unwrap(),
            rel_path: (*path).into(),
            size: bytes.len() as u64,
            sha256_hex: hex_encode(&digest.content_sha256),
            metadata: content.is_none().then(|| EntryMetadata {
                file_kind: FileKind::Directory,
                ..EntryMetadata::default()
            }),
            members: vec![],
        });
        digests.push(digest);
    }
    (entries, flat_merkle_root_from_digests(&digests))
}

fn commitment(entries: &[ManifestEntry]) -> String {
    let regular = EntryMetadata::default();
    let pairs: Vec<_> = entries
        .iter()
        .map(|entry| {
            (
                entry.rel_path.as_str(),
                entry.metadata.as_ref().unwrap_or(&regular),
            )
        })
        .collect();
    // The all-regular case is used only to check refusal of noncanonical input.
    metadata_commitment(&pairs).unwrap_or_else(|| "00".repeat(32))
}

fn checked(entries: Vec<ManifestEntry>, root: String) -> Result<DirectoryManifest, Error> {
    let metadata_root = commitment(&entries);
    DirectoryManifest::with_metadata(
        "project".into(),
        entries.iter().map(|entry| entry.size).sum(),
        entries,
        root,
        metadata_root,
    )
}

fn structural(items: &[Item<'_>]) -> DirectoryManifest {
    let (entries, root) = parts(items);
    checked(entries, root).unwrap()
}

fn staging(scratch: &Scratch) -> PathBuf {
    fs::read_dir(&scratch.0)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(".fr-part-")
        })
        .unwrap()
}

#[test]
fn mixed_empty_directories_and_files_publish_as_the_original_private_tree() {
    for conflict in [false, true] {
        let scratch = Scratch::new();
        let root = scratch
            .open()
            .with_conflict_policy(ConflictPolicy::KeepBoth);
        let manifest = structural(&[
            ("empty-sibling", None),
            ("src/deep/empty-leaf", None),
            ("src/data", Some(b"abc")),
            ("zero-file", Some(b"")),
        ]);
        assert_eq!(manifest.file_count(), 4); // ATP counts declared entries.
        assert_eq!(manifest.total_bytes(), 3);
        let mut pending = root.begin_directory(manifest).unwrap();
        pending.write_entry(2, 0, b"abc").unwrap();
        let staged = staging(&scratch);
        let before = fs::metadata(staged.join("src/deep/empty-leaf")).unwrap();
        assert!(!scratch.0.join("project").exists());
        pending.verify().unwrap();
        assert!(!scratch.0.join("project").exists());
        if conflict {
            fs::create_dir(scratch.0.join("project")).unwrap();
            fs::write(scratch.0.join("project/precious"), b"keep").unwrap();
        }
        let receipt = pending.publish_named().unwrap();
        assert_eq!(receipt.publication(), Publication::Durable);
        assert_eq!(receipt.was_renamed(), conflict);
        let published = scratch.0.join(receipt.name());
        assert_eq!(fs::read(published.join("src/data")).unwrap(), b"abc");
        assert!(published.join("zero-file").is_file());
        assert_eq!(fs::metadata(published.join("zero-file")).unwrap().len(), 0);
        for path in [
            "",
            "empty-sibling",
            "src",
            "src/deep",
            "src/deep/empty-leaf",
        ] {
            let metadata = fs::metadata(published.join(path)).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        }
        for path in ["empty-sibling", "src/deep/empty-leaf"] {
            assert_eq!(fs::read_dir(published.join(path)).unwrap().count(), 0);
        }
        let after = fs::metadata(published.join("src/deep/empty-leaf")).unwrap();
        assert_eq!((before.dev(), before.ino()), (after.dev(), after.ino()));
        assert!(!staged.exists());
        if conflict {
            assert_eq!(
                fs::read(scratch.0.join("project/precious")).unwrap(),
                b"keep"
            );
            assert_eq!(fs::read_dir(scratch.0.join("project")).unwrap().count(), 1);
        }
    }
}

#[test]
fn an_all_empty_nested_tree_needs_no_data_but_still_verifies_its_content_root() {
    for corrupt in [false, true] {
        let scratch = Scratch::new();
        let (entries, mut root) = parts(&[("one", None), ("two/deep/leaf", None)]);
        if corrupt {
            root = "00".repeat(32);
        }
        let mut pending = scratch
            .open()
            .begin_directory(checked(entries, root).unwrap())
            .unwrap();
        assert_eq!(pending.received_bytes(), 0);
        if corrupt {
            assert_eq!(pending.verify(), Err(Error::Integrity));
            drop(pending);
            assert_eq!(scratch.entries(), 0);
        } else {
            pending.verify().unwrap();
            pending.publish().unwrap();
            assert!(scratch.0.join("project/one").is_dir());
            assert!(scratch.0.join("project/two/deep/leaf").is_dir());
            assert_eq!(
                fs::read_dir(scratch.0.join("project/two/deep/leaf"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }
}

#[test]
fn a_directory_index_never_accepts_data_or_becomes_an_empty_regular_file() {
    for bytes in [&b""[..], &b"x"[..]] {
        let scratch = Scratch::new();
        let mut pending = scratch
            .open()
            .begin_directory(structural(&[("leaf", None), ("later-file", Some(b"x"))]))
            .unwrap();
        assert_eq!(pending.write_entry(0, 0, bytes), Err(Error::InvalidChunk));
        assert_eq!(pending.write_entry(1, 0, b"x"), Err(Error::Retired));
        assert_eq!(pending.verify(), Err(Error::Retired));
        drop(pending);
        assert_eq!(scratch.entries(), 0);
    }
}

#[test]
fn kinds_names_and_every_regular_entry_are_bound_before_creating_staging() {
    let scratch = Scratch::new();
    let (entries, root) = parts(&[("leaf", None), ("file", Some(b""))]);
    let metadata_root = commitment(&entries);
    for mutation in 0..4 {
        let mut changed = entries.clone();
        match mutation {
            0 => changed[0].metadata = None,
            1 => changed[0].rel_path = "different-directory".into(),
            2 => changed[1].rel_path = "different-file".into(),
            _ => changed[1].metadata = changed[0].metadata.clone(),
        }
        assert_eq!(
            DirectoryManifest::with_metadata(
                "project".into(),
                0,
                changed,
                root.clone(),
                metadata_root.clone()
            )
            .unwrap_err(),
            Error::Integrity
        );
    }
    assert!(DirectoryManifest::new("project".into(), 0, entries, root).is_err());
    assert_eq!(scratch.entries(), 0);
}

#[test]
fn unsupported_metadata_cannot_be_authenticated_into_native_effects() {
    let edits: [fn(&mut EntryMetadata); 12] = [
        |m| m.file_kind = FileKind::Regular,
        |m| m.file_kind = FileKind::Symlink,
        |m| m.unix_mode = Some(0o777),
        |m| m.mtime_unix_secs = Some(0),
        |m| m.mtime_nanos = Some(0),
        |m| m.uid = Some(0),
        |m| m.gid = Some(0),
        |m| m.windows_attributes = Some(0),
        |m| m.symlink_target = Some("outside".into()),
        |m| m.hardlink_target = Some("outside".into()),
        |m| {
            m.xattrs.insert("user.private".into(), vec![1]);
        },
        |m| {
            m.symlink_target_info = Some(
                serde_json::from_value(serde_json::json!({"semantics":"portable_relative"}))
                    .unwrap(),
            )
        },
    ];
    for edit in edits {
        let (mut entries, root) = parts(&[("leaf", None)]);
        edit(entries[0].metadata.as_mut().unwrap());
        // Recompute the real upstream commitment: metadata must be forbidden
        // even when the peer can correctly authenticate its unsupported intent.
        assert!(checked(entries, root).is_err());
    }
    for wrong_hash in [false, true] {
        let (mut entries, root) = parts(&[("leaf", None)]);
        if wrong_hash {
            entries[0].sha256_hex = "00".repeat(32);
        } else {
            entries[0].size = 1;
        }
        assert_eq!(checked(entries, root).unwrap_err(), Error::Integrity);
    }
}

#[test]
fn explicit_leaves_reject_aliases_ancestors_traversal_and_zero_byte_quota_evasion() {
    for items in [
        vec![("a", None), ("a", None)],
        vec![("a", None), ("a", Some(&b""[..]))],
        vec![("a", None), ("a/b", Some(&b""[..]))],
        vec![("a/b", None), ("a", None)],
        vec![("A/x", None), ("a/y", None)],
        vec![("../escape", None)],
        vec![("a/.fr-part-owned", None)],
        vec![("a/b/c/d/e/f/g/h/i", None)],
    ] {
        let (entries, root) = parts(&items);
        assert!(checked(entries, root).is_err());
    }
    for kind in 0..3 {
        let names: Vec<_> = (0..if kind == 0 { 65 } else { 64 })
            .map(|i| match kind {
                0 => format!("leaf{i}"),
                1 => format!("d{i}/nested/leaf"),
                _ => format!("{}/leaf{i:03}{}", "a".repeat(255), "x".repeat(14)),
            })
            .collect();
        let items: Vec<_> = names.iter().map(|name| (name.as_str(), None)).collect();
        let (entries, root) = parts(&items);
        assert!(checked(entries, root).is_err());
    }
}

#[test]
fn cancellation_removes_only_owned_structural_nodes_and_retains_failed_custody() {
    for obstruct in [false, true] {
        let scratch = Scratch::new();
        let root = scratch.open_with_limits(Limits {
            max_file_bytes: 3,
            max_reserved_bytes: 3,
            max_transfers: 1,
        });
        let mut pending = root
            .begin_directory(structural(&[("a/leaf", None), ("file", Some(b"abc"))]))
            .unwrap();
        pending.write_entry(1, 0, b"abc").unwrap();
        let staged = staging(&scratch);
        let unowned = staged.join("a/leaf/local-only");
        if obstruct {
            fs::write(&unowned, b"keep").unwrap();
        }
        let outcome = pending.cancel();
        if obstruct {
            assert!(outcome.is_err());
            assert_eq!(fs::read(&unowned).unwrap(), b"keep");
            assert!(!staged.join("file").exists());
            assert!(matches!(
                root.begin_directory(structural(&[("another", None)])),
                Err(Error::Quota)
            ));
        } else {
            outcome.unwrap();
            assert_eq!(scratch.entries(), 0);
            root.begin_directory(structural(&[("another", None)]))
                .unwrap()
                .cancel()
                .unwrap();
        }
        assert!(!scratch.0.join("project").exists());
    }
}
