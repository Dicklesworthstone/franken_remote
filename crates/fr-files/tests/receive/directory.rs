use super::*;
use asupersync::net::atp::{
    transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
    transport_tcp::ManifestEntry,
};
use fr_files::receive::DirectoryManifest;
fn manifest(paths: &[(&str, &[u8])]) -> DirectoryManifest {
    let mut digests = Vec::new();
    let mut entries = Vec::new();
    for (index, (path, bytes)) in paths.iter().enumerate() {
        let mut hash = StagedEntryReceive::new("hash-only".into());
        hash.update_with_chunk(bytes);
        let (digest, _, _) = hash.finalize((*path).into());
        entries.push(ManifestEntry {
            index: index as u32,
            rel_path: (*path).into(),
            size: bytes.len() as u64,
            sha256_hex: hex_encode(&digest.content_sha256),
            metadata: None,
            members: vec![],
        });
        digests.push(digest);
    }
    DirectoryManifest::new(
        "project".into(),
        paths.iter().map(|(_, b)| b.len() as u64).sum(),
        entries,
        flat_merkle_root_from_digests(&digests),
    )
    .unwrap()
}
#[test]
fn nested_tree_and_empty_files_publish_together_with_private_permissions() {
    let scratch = Scratch::new();
    let mut p = scratch
        .open()
        .begin_directory(manifest(&[
            ("src/a", b"abc"),
            ("src/deep/b", b"xyz"),
            ("empty", b""),
        ]))
        .unwrap();
    // Entry offsets are independent; network interleaving is permitted but a
    // duplicate offset cannot overwrite already staged content.
    p.write_entry(1, 0, b"xy").unwrap();
    p.write_entry(0, 0, b"abc").unwrap();
    p.write_entry(1, 2, b"z").unwrap();
    assert_eq!(p.received_bytes(), 6);
    assert!(!scratch.0.join("project").exists());
    p.verify().unwrap();
    assert!(!scratch.0.join("project").exists());
    assert_eq!(p.publish().unwrap(), Publication::Durable);
    assert_eq!(fs::read(scratch.0.join("project/src/a")).unwrap(), b"abc");
    assert_eq!(
        fs::read(scratch.0.join("project/src/deep/b")).unwrap(),
        b"xyz"
    );
    assert_eq!(
        fs::metadata(scratch.0.join("project/empty")).unwrap().len(),
        0
    );
    for path in ["project", "project/src", "project/src/deep"] {
        assert_eq!(
            fs::metadata(scratch.0.join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert_eq!(
        fs::metadata(scratch.0.join("project/src/a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(scratch.entries(), 1);
}
#[test]
fn empty_root_is_verified_and_published() {
    let scratch = Scratch::new();
    let mut p = scratch.open().begin_directory(manifest(&[])).unwrap();
    p.verify().unwrap();
    p.publish().unwrap();
    assert!(scratch.0.join("project").is_dir());
    assert_eq!(fs::read_dir(scratch.0.join("project")).unwrap().count(), 0);
}
#[test]
fn cancel_and_corrupt_entry_reclaim_the_entire_private_tree() {
    let scratch = Scratch::new();
    let root = scratch.open();
    for corrupt in [false, true] {
        let mut p = root
            .begin_directory(manifest(&[("src/a", b"abc"), ("src/deep/b", b"")]))
            .unwrap();
        if corrupt {
            p.write_entry(0, 0, b"bad").unwrap();
            assert_eq!(p.verify(), Err(Error::Integrity));
        }
        p.cancel().unwrap();
        assert_eq!(scratch.entries(), 0);
    }
    let mut p = root.begin_directory(manifest(&[("a", b"abc")])).unwrap();
    assert_eq!(p.verify(), Err(Error::Incomplete));
    drop(p);
    assert_eq!(scratch.entries(), 0);
}
#[test]
fn existing_directory_file_and_symlink_are_never_replaced() {
    for kind in 0..3 {
        let scratch = Scratch::new();
        let mut p = scratch
            .open()
            .begin_directory(manifest(&[("a", b"abc")]))
            .unwrap();
        p.write_entry(0, 0, b"abc").unwrap();
        p.verify().unwrap();
        let dest = scratch.0.join("project");
        match kind {
            0 => {
                fs::create_dir(&dest).unwrap();
                fs::write(dest.join("precious"), b"keep").unwrap();
            }
            1 => fs::write(&dest, b"keep").unwrap(),
            _ => symlink("missing", &dest).unwrap(),
        }
        assert_eq!(p.publish(), Err(Error::Conflict));
        match kind {
            0 => assert_eq!(fs::read(dest.join("precious")).unwrap(), b"keep"),
            1 => assert_eq!(fs::read(dest).unwrap(), b"keep"),
            _ => assert_eq!(fs::read_link(dest).unwrap(), PathBuf::from("missing")),
        }
        assert_eq!(scratch.entries(), 1);
    }
}
#[test]
fn bounds_include_zero_byte_entries_and_implicit_parent_nodes() {
    let entry = |i: u32, name: String| ManifestEntry {
        index: i,
        rel_path: name,
        size: 0,
        sha256_hex: "00".repeat(32),
        metadata: None,
        members: vec![],
    };
    let too_many: Vec<_> = (0..65).map(|i| entry(i, format!("f{i}"))).collect();
    assert!(DirectoryManifest::new("project".into(), 0, too_many, "00".repeat(32)).is_err());
    let too_many_nodes: Vec<_> = (0..64)
        .map(|i| entry(i, format!("d{i}/nested/file")))
        .collect();
    assert!(DirectoryManifest::new("project".into(), 0, too_many_nodes, "00".repeat(32)).is_err());
    let too_deep = vec![entry(0, "a/b/c/d/e/f/g/h/i".into())];
    assert!(DirectoryManifest::new("project".into(), 0, too_deep, "00".repeat(32)).is_err());
}
#[test]
fn traversal_reserved_names_duplicates_and_file_parent_aliases_refuse_before_disk() {
    for paths in [
        vec!["../escape"],
        vec!["/escape"],
        vec!["a//b"],
        vec!["a/./b"],
        vec!["a/../b"],
        vec!["a\\b"],
        vec!["CON"],
        vec!["a/.fr-part-owned"],
        vec!["a", "a"],
        vec!["a", "a/b"],
        vec!["a/b", "a"],
        vec!["A/x", "a/y"],
    ] {
        let entries: Vec<_> = paths
            .iter()
            .enumerate()
            .map(|(i, p)| ManifestEntry {
                index: i as u32,
                rel_path: (*p).into(),
                size: 0,
                sha256_hex: "00".repeat(32),
                metadata: None,
                members: vec![],
            })
            .collect();
        assert!(
            DirectoryManifest::new("project".into(), 0, entries, "00".repeat(32)).is_err(),
            "{paths:?}"
        );
    }
}
#[test]
fn invalid_index_duplicate_or_overlong_chunk_retires_the_object() {
    for (index, offset, bytes) in [(100, 0, &b"a"[..]), (0, 1, &b"a"[..]), (0, 0, &b"abcd"[..])] {
        let scratch = Scratch::new();
        let mut p = scratch
            .open()
            .begin_directory(manifest(&[("a", b"abc")]))
            .unwrap();
        assert_eq!(
            p.write_entry(index, offset, bytes),
            Err(Error::InvalidChunk)
        );
        assert_eq!(p.write_entry(0, 0, b"abc"), Err(Error::Retired));
        drop(p);
        assert_eq!(scratch.entries(), 0);
    }
}
#[test]
fn directory_reservation_is_aggregate_and_released_on_cancel() {
    let scratch = Scratch::new();
    let root = scratch.open_with_limits(Limits {
        max_file_bytes: 4,
        max_reserved_bytes: 4,
        max_transfers: 1,
    });
    assert!(matches!(
        root.begin_directory(manifest(&[("a", b"abc"), ("b", b"def")])),
        Err(Error::Quota)
    ));
    let p = root.begin_directory(manifest(&[("a", b"abc")])).unwrap();
    assert!(matches!(
        root.begin_directory(manifest(&[])),
        Err(Error::Quota)
    ));
    p.cancel().unwrap();
    root.begin_directory(manifest(&[]))
        .unwrap()
        .cancel()
        .unwrap();
    assert_eq!(scratch.entries(), 0);
}
