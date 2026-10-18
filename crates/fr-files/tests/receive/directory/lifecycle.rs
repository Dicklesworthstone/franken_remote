//! Verify actual local outcomes, retained directory identity and shared custody.
use super::*;
use fr_files::receive::{ConflictPolicy, PublishedDirectory};
use std::os::unix::fs::MetadataExt;

fn empty_named(name: &str) -> DirectoryManifest {
    DirectoryManifest::new(name.into(), 0, vec![], flat_merkle_root_from_digests(&[])).unwrap()
}

#[test]
fn named_tree_receipt_matches_the_original_verified_inodes_without_copying() {
    for conflict in [false, true] {
        let scratch = Scratch::new();
        let root = scratch
            .open()
            .with_conflict_policy(ConflictPolicy::KeepBoth);
        let mut pending = root
            .begin_directory(manifest(&[("src/a", b"abc"), ("src/empty", b"")]))
            .unwrap();
        pending.write_entry(0, 0, b"abc").unwrap();
        pending.verify().unwrap();
        let staging = fs::read_dir(&scratch.0)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let staged_root = fs::metadata(&staging).unwrap();
        let staged_file = fs::metadata(staging.join("src/a")).unwrap();
        if conflict {
            fs::write(scratch.0.join("project"), b"original").unwrap();
        }
        let result: PublishedDirectory = pending.publish_named().unwrap();
        assert_eq!(result.was_renamed(), conflict);
        assert_eq!(result.publication(), Publication::Durable);
        if conflict {
            assert_ne!(result.name(), "project");
            assert_eq!(fs::read(scratch.0.join("project")).unwrap(), b"original");
        } else {
            assert_eq!(result.name(), "project");
        }
        let published = scratch.0.join(result.name());
        let final_root = fs::metadata(&published).unwrap();
        let final_file = fs::metadata(published.join("src/a")).unwrap();
        assert_eq!(
            (final_root.dev(), final_root.ino()),
            (staged_root.dev(), staged_root.ino())
        );
        assert_eq!(
            (final_file.dev(), final_file.ino()),
            (staged_file.dev(), staged_file.ino())
        );
        assert_eq!(final_root.permissions().mode() & 0o777, 0o700);
        assert_eq!(final_file.permissions().mode() & 0o777, 0o600);
        assert_eq!(fs::read(published.join("src/a")).unwrap(), b"abc");
        assert_eq!(fs::read(published.join("src/empty")).unwrap(), b"");
        assert!(!staging.exists());
        // The name-bearing result is local UI data, not diagnostic payload.
        let debug = format!("{result:?}");
        assert!(!debug.contains("project"));
        assert!(!debug.contains("fr-conflict"));
        assert!(!debug.contains(result.name()));
    }
}

#[test]
fn replacing_the_selected_path_does_not_redirect_tree_conflict_publication() {
    let scratch = Scratch::new();
    let selected = scratch.0.join("selected");
    let moved = scratch.0.join("moved");
    fs::create_dir(&selected).unwrap();
    fs::set_permissions(&selected, fs::Permissions::from_mode(0o700)).unwrap();
    let root = DropDirectory::open(
        &selected,
        Limits {
            max_file_bytes: 16,
            max_reserved_bytes: 16,
            max_transfers: 1,
        },
    )
    .unwrap()
    .with_conflict_policy(ConflictPolicy::KeepBoth);
    let mut pending = root.begin_directory(manifest(&[("a", b"abc")])).unwrap();
    pending.write_entry(0, 0, b"abc").unwrap();
    pending.verify().unwrap();
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(selected.join("replacement-only"), b"replacement").unwrap();
    fs::write(moved.join("project"), b"original").unwrap();
    let result = pending.publish_named().unwrap();
    assert!(result.was_renamed());
    assert_eq!(
        fs::read(moved.join(result.name()).join("a")).unwrap(),
        b"abc"
    );
    assert_eq!(fs::read(moved.join("project")).unwrap(), b"original");
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 2);
    assert_eq!(fs::read_dir(&selected).unwrap().count(), 1);
    assert_eq!(
        fs::read(selected.join("replacement-only")).unwrap(),
        b"replacement"
    );
}

#[test]
fn policy_is_local_per_clone_but_file_and_tree_reservations_are_shared() {
    let scratch = Scratch::new();
    let reject = scratch.open_with_limits(Limits {
        max_file_bytes: 3,
        max_reserved_bytes: 3,
        max_transfers: 1,
    });
    let keep = reject
        .clone()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    fs::write(scratch.0.join("project"), b"original").unwrap();
    let mut pending = keep.begin_directory(manifest(&[("a", b"abc")])).unwrap();
    pending.write_entry(0, 0, b"abc").unwrap();
    pending.verify().unwrap();
    // Changing a clone's conflict preference cannot reset a live reservation.
    assert!(matches!(
        reject.begin_directory(manifest(&[])),
        Err(Error::Quota)
    ));
    assert!(matches!(
        keep.begin("file", 0, ContentId::from_bytes(b"")),
        Err(Error::Quota)
    ));
    let published = pending.publish_named().unwrap();
    assert!(published.was_renamed());
    let mut denied = reject.begin_directory(manifest(&[])).unwrap();
    denied.verify().unwrap();
    assert_eq!(denied.publish_named().unwrap_err(), Error::Conflict);
    let file = reject
        .begin("file", 3, ContentId::from_bytes(b"abc"))
        .unwrap();
    assert!(matches!(
        keep.begin_directory(manifest(&[])),
        Err(Error::Quota)
    ));
    file.cancel().unwrap();
    keep.begin_directory(manifest(&[]))
        .unwrap()
        .cancel()
        .unwrap();
    assert_eq!(scratch.entries(), 2);
    assert_eq!(fs::read(scratch.0.join("project")).unwrap(), b"original");
    assert_eq!(
        fs::read(scratch.0.join(published.name()).join("a")).unwrap(),
        b"abc"
    );
}

#[test]
fn cancelled_verified_tree_does_not_publish_or_touch_legacy_entries() {
    let scratch = Scratch::new();
    let keep = scratch
        .open_with_limits(Limits {
            max_file_bytes: 3,
            max_reserved_bytes: 3,
            max_transfers: 1,
        })
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    fs::write(scratch.0.join("project"), b"original").unwrap();
    fs::write(scratch.0.join(".fr-part-legacy"), b"legacy").unwrap();
    for explicit in [false, true] {
        let mut pending = keep.begin_directory(manifest(&[("a", b"abc")])).unwrap();
        pending.write_entry(0, 0, b"abc").unwrap();
        pending.verify().unwrap();
        if explicit {
            pending.cancel().unwrap();
        } else {
            drop(pending);
        }
        assert_eq!(scratch.entries(), 2);
        assert_eq!(fs::read(scratch.0.join("project")).unwrap(), b"original");
        assert_eq!(
            fs::read(scratch.0.join(".fr-part-legacy")).unwrap(),
            b"legacy"
        );
    }
    keep.begin_directory(manifest(&[]))
        .unwrap()
        .cancel()
        .unwrap();
}

#[test]
fn empty_trees_and_maximum_length_unicode_conflict_names_remain_portable() {
    for name in [
        ".project".into(),
        format!("{}a", "é".repeat(127)),
        format!("{}.zip", "漢".repeat(83)),
    ] {
        let scratch = Scratch::new();
        let root = scratch
            .open()
            .with_conflict_policy(ConflictPolicy::KeepBoth);
        fs::write(scratch.0.join(&name), b"original").unwrap();
        let mut pending = root.begin_directory(empty_named(&name)).unwrap();
        pending.verify().unwrap();
        let result = pending.publish_named().unwrap();
        assert!(result.was_renamed());
        assert_ne!(result.name(), name);
        assert!(result.name().len() <= 255);
        assert!(asupersync::atp::safety::validate_portable_path_component(result.name()).is_ok());
        assert_eq!(
            fs::read_dir(scratch.0.join(result.name())).unwrap().count(),
            0
        );
        assert_eq!(fs::read(scratch.0.join(&name)).unwrap(), b"original");
        assert_eq!(scratch.entries(), 2);
    }
}

#[test]
fn keep_both_cannot_publish_valid_file_hashes_with_a_wrong_tree_root() {
    let scratch = Scratch::new();
    let mut hash = StagedEntryReceive::new("hash-only".into());
    hash.update_with_chunk(b"");
    let (digest, _, _) = hash.finalize("src/empty".into());
    let manifest = DirectoryManifest::new(
        "project".into(),
        0,
        vec![ManifestEntry {
            index: 0,
            rel_path: "src/empty".into(),
            size: 0,
            sha256_hex: hex_encode(&digest.content_sha256),
            metadata: None,
            members: vec![],
        }],
        "00".repeat(32),
    )
    .unwrap();
    fs::write(scratch.0.join("project"), b"original").unwrap();
    let root = scratch
        .open()
        .with_conflict_policy(ConflictPolicy::KeepBoth);
    let mut pending = root.begin_directory(manifest).unwrap();
    assert_eq!(pending.verify(), Err(Error::Integrity));
    assert_eq!(pending.publish_named().unwrap_err(), Error::Retired);
    assert_eq!(scratch.entries(), 1);
    assert_eq!(fs::read(scratch.0.join("project")).unwrap(), b"original");
}
