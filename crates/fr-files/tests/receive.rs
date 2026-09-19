#![cfg(target_os = "linux")]
use asupersync::atp::object::ContentId;
use fr_files::receive::{DropDirectory, Error, Limits, MAX_CHUNK_BYTES, Publication};
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
    fn entries(&self) -> usize {
        fs::read_dir(&self.0).unwrap().count()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn streams_real_multichunk_object_without_exposing_partial_destination() {
    let scratch = Scratch::new();
    let root = scratch.open();
    let data = vec![0x5a; MAX_CHUNK_BYTES * 3 + 11];
    let mut transfer = root
        .begin(
            "report.bin",
            data.len() as u64,
            ContentId::from_bytes(&data),
        )
        .unwrap();
    for chunk in data.chunks(MAX_CHUNK_BYTES) {
        transfer
            .write_chunk(transfer.received_bytes(), chunk)
            .unwrap();
        assert!(!scratch.0.join("report.bin").exists());
    }
    transfer.verify().unwrap();
    assert!(!scratch.0.join("report.bin").exists());
    assert_eq!(transfer.publish().unwrap(), Publication::Durable);
    assert_eq!(fs::read(scratch.0.join("report.bin")).unwrap(), data);
    assert_eq!(scratch.entries(), 1);
    assert_eq!(
        fs::metadata(scratch.0.join("report.bin"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn empty_object_is_integrity_checked_and_published() {
    let scratch = Scratch::new();
    let mut transfer = scratch
        .open()
        .begin("empty", 0, ContentId::from_bytes(b""))
        .unwrap();
    transfer.verify().unwrap();
    assert_eq!(transfer.publish().unwrap(), Publication::Durable);
    assert_eq!(fs::metadata(scratch.0.join("empty")).unwrap().len(), 0);
}

#[test]
fn traversal_devices_aliases_and_staging_namespace_are_refused_before_io() {
    let scratch = Scratch::new();
    let root = scratch.open();
    let too_long = "a".repeat(256);
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "/absolute",
        "a/b",
        "a\\b",
        "NUL.txt",
        "COM1",
        "a:stream",
        "a.",
        "a ",
        "a\0b",
        ".fr-part-claimed",
        &too_long,
    ] {
        assert_eq!(
            root.begin(name, 0, ContentId::from_bytes(b"")).unwrap_err(),
            Error::InvalidName
        );
        assert_eq!(scratch.entries(), 0);
    }
}

#[test]
fn checksum_failure_incomplete_and_bad_offsets_never_publish() {
    let scratch = Scratch::new();
    let root = scratch.open();
    let mut bad_hash = root
        .begin("hash", 3, ContentId::from_bytes(b"abc"))
        .unwrap();
    bad_hash.write_chunk(0, b"abd").unwrap();
    assert_eq!(bad_hash.verify(), Err(Error::Integrity));
    assert_eq!(bad_hash.publish(), Err(Error::Retired));
    let mut short = root
        .begin("short", 3, ContentId::from_bytes(b"abc"))
        .unwrap();
    short.write_chunk(0, b"a").unwrap();
    assert_eq!(short.verify(), Err(Error::Incomplete));
    drop(short);
    let mut offset = root
        .begin("offset", 3, ContentId::from_bytes(b"abc"))
        .unwrap();
    assert_eq!(offset.write_chunk(1, b"a"), Err(Error::InvalidChunk));
    assert_eq!(offset.write_chunk(0, b"abc"), Err(Error::Retired));
    drop(offset);
    assert_eq!(scratch.entries(), 0);
}

#[test]
fn oversized_empty_replayed_and_past_end_chunks_are_terminal() {
    let scratch = Scratch::new();
    let root = scratch.open();
    for data in [Vec::new(), vec![0; MAX_CHUNK_BYTES + 1], vec![0; 4]] {
        let mut transfer = root.begin("bad", 3, ContentId::from_bytes(b"abc")).unwrap();
        assert_eq!(transfer.write_chunk(0, &data), Err(Error::InvalidChunk));
        assert_eq!(transfer.verify(), Err(Error::Retired));
    }
    let mut replay = root
        .begin("replay", 3, ContentId::from_bytes(b"abc"))
        .unwrap();
    replay.write_chunk(0, b"a").unwrap();
    assert_eq!(replay.write_chunk(0, b"a"), Err(Error::InvalidChunk));
    drop(replay);
    assert_eq!(scratch.entries(), 0);
}

#[test]
fn racing_existing_file_or_symlink_is_never_overwritten_or_followed() {
    let scratch = Scratch::new();
    let outside = Scratch::new();
    fs::write(outside.0.join("untouched"), b"local data").unwrap();
    let root = scratch.open();
    for is_link in [false, true] {
        let name = if is_link { "link" } else { "file" };
        let mut transfer = root.begin(name, 3, ContentId::from_bytes(b"abc")).unwrap();
        transfer.write_chunk(0, b"abc").unwrap();
        transfer.verify().unwrap();
        if is_link {
            symlink(outside.0.join("untouched"), scratch.0.join(name)).unwrap();
        } else {
            fs::write(scratch.0.join(name), b"local data").unwrap();
        }
        assert_eq!(transfer.publish(), Err(Error::Conflict));
        assert_eq!(fs::read(scratch.0.join(name)).unwrap(), b"local data");
    }
    assert_eq!(
        fs::read(outside.0.join("untouched")).unwrap(),
        b"local data"
    );
    assert_eq!(scratch.entries(), 2);
}

#[test]
fn root_symlinks_and_foreign_writable_directory_are_refused() {
    let scratch = Scratch::new();
    let outside = Scratch::new();
    symlink(&outside.0, scratch.0.join("redirect")).unwrap();
    fs::create_dir(outside.0.join("subdir")).unwrap();
    let limits = Limits {
        max_file_bytes: 10,
        max_reserved_bytes: 10,
        max_transfers: 1,
    };
    assert!(DropDirectory::open(&scratch.0.join("redirect"), limits).is_err());
    assert!(DropDirectory::open(&scratch.0.join("redirect/subdir"), limits).is_err());
    assert_eq!(
        DropDirectory::open(std::path::Path::new("relative"), limits).unwrap_err(),
        Error::InvalidRoot
    );
    fs::set_permissions(&outside.0, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
        DropDirectory::open(&outside.0, limits).unwrap_err(),
        Error::UnsafeRoot
    );
}

#[test]
fn replacing_root_path_does_not_redirect_descriptor_relative_publication() {
    let scratch = Scratch::new();
    let drop_path = scratch.0.join("drop");
    fs::create_dir(&drop_path).unwrap();
    fs::set_permissions(&drop_path, fs::Permissions::from_mode(0o700)).unwrap();
    let limits = Limits {
        max_file_bytes: 10,
        max_reserved_bytes: 10,
        max_transfers: 1,
    };
    let root = DropDirectory::open(&drop_path, limits).unwrap();
    let mut transfer = root
        .begin("result", 3, ContentId::from_bytes(b"abc"))
        .unwrap();
    fs::rename(&drop_path, scratch.0.join("original")).unwrap();
    fs::create_dir(&drop_path).unwrap();
    fs::set_permissions(&drop_path, fs::Permissions::from_mode(0o700)).unwrap();
    transfer.write_chunk(0, b"abc").unwrap();
    transfer.verify().unwrap();
    transfer.publish().unwrap();
    assert!(!drop_path.join("result").exists());
    assert_eq!(fs::read(scratch.0.join("original/result")).unwrap(), b"abc");
}

#[test]
fn reservations_are_shared_and_reclaimed_after_cancellation() {
    let scratch = Scratch::new();
    let root = scratch.open();
    assert_eq!(
        root.begin("huge", u64::MAX, ContentId::from_bytes(b""))
            .unwrap_err(),
        Error::Quota
    );
    let first = root
        .begin("one", 700_000, ContentId::from_bytes(b""))
        .unwrap();
    assert_eq!(
        root.clone()
            .begin("two", 700_000, ContentId::from_bytes(b""))
            .unwrap_err(),
        Error::Quota
    );
    let second = root
        .clone()
        .begin("two", 0, ContentId::from_bytes(b""))
        .unwrap();
    assert_eq!(
        root.begin("three", 0, ContentId::from_bytes(b""))
            .unwrap_err(),
        Error::Quota
    );
    first.cancel().unwrap();
    let replacement = root
        .begin("three", 700_000, ContentId::from_bytes(b""))
        .unwrap();
    drop((second, replacement));
    assert_eq!(scratch.entries(), 0);
}

#[test]
fn logs_and_errors_do_not_include_private_metadata() {
    let scratch = Scratch::new();
    let root = scratch.open();
    let transfer = root
        .begin(
            "confidential-finances.txt",
            6,
            ContentId::from_bytes(b"secret"),
        )
        .unwrap();
    let output = format!("{root:?} {transfer:?} {}", Error::Integrity);
    assert!(!output.contains("confidential"));
    assert!(!output.contains("secret"));
    assert!(!output.contains(scratch.0.to_str().unwrap()));
}
