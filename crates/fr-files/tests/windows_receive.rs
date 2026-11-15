#![cfg(windows)]
//! Real Windows filesystem custody and the actual ATP disk worker. These tests
//! do not claim a live tailnet transfer, independent wire interoperability, or
//! durable directory metadata when the platform cannot establish it.
use asupersync::atp::object::ContentId;
use fr_files::receive::{
    ConflictPolicy, DirectoryManifest, DropDirectory, Error, Limits, MAX_CHUNK_BYTES, PendingFile,
    Publication,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "fr-windows-receive-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn open(&self) -> DropDirectory {
        DropDirectory::open(&self.0, limits()).unwrap()
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

fn limits() -> Limits {
    Limits {
        max_file_bytes: 1024 * 1024,
        max_reserved_bytes: 1024 * 1024,
        max_transfers: 1,
    }
}

fn verified(root: &DropDirectory, name: &str, bytes: &[u8]) -> PendingFile {
    let mut pending = root
        .begin(name, bytes.len() as u64, ContentId::from_bytes(bytes))
        .unwrap();
    for chunk in bytes.chunks(MAX_CHUNK_BYTES) {
        pending
            .write_chunk(pending.received_bytes(), chunk)
            .unwrap();
    }
    pending.verify().unwrap();
    pending
}

fn grant_everyone(path: &Path, rights: &str) {
    let result = Command::new("icacls")
        .arg(path)
        .args(["/grant", &format!("*S-1-1-0:{rights}")])
        .output()
        .unwrap();
    assert!(result.status.success());
}

fn junction(path: &Path, target: &Path) {
    let result = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(path)
        .arg(target)
        .output()
        .unwrap();
    assert!(result.status.success());
}

#[test]
fn multichunk_publication_is_private_complete_and_truthful_about_durability() {
    let scratch = Scratch::new();
    // A read grant on the selected root must never be inherited by new content.
    grant_everyone(&scratch.0, "(OI)(CI)(R)");
    let root = scratch.open();
    let bytes = vec![0x5a; MAX_CHUNK_BYTES * 3 + 11];
    let mut pending = root
        .begin("complete.bin", bytes.len() as u64, ContentId::from_bytes(&bytes))
        .unwrap();
    for chunk in bytes.chunks(MAX_CHUNK_BYTES) {
        pending
            .write_chunk(pending.received_bytes(), chunk)
            .unwrap();
        assert!(!scratch.0.join("complete.bin").exists());
    }
    pending.verify().unwrap();
    assert!(!scratch.0.join("complete.bin").exists());
    assert_eq!(pending.publish(), Ok(Publication::DurabilityUnknown));
    // The return occurs after closing the original no-sharing staging handle.
    assert_eq!(fs::read(scratch.0.join("complete.bin")).unwrap(), bytes);
    let published = fr_winsys::fs::open_no_follow(
        &scratch.0.join("complete.bin"),
        fr_winsys::fs::Open::Read,
    )
    .unwrap();
    let inspection = fr_winsys::fs::inspect(&published).unwrap();
    assert_eq!(inspection.owner, fr_winsys::fs::Owner::Us);
    assert!(!inspection.others_may_access());
    assert!(!inspection.directory && !inspection.reparse);
    assert_eq!(inspection.links, 1);
    assert_eq!(scratch.entries(), 1);
    // Successful publication closed its file and released its reservation.
    verified(&root, "next", b"next").cancel().unwrap();
}

#[test]
fn an_empty_file_is_verified_then_published_without_directory_durability_claims() {
    let scratch = Scratch::new();
    let root = scratch.open();
    assert_eq!(
        verified(&root, "empty", b"").publish(),
        Ok(Publication::DurabilityUnknown)
    );
    assert_eq!(fs::metadata(scratch.0.join("empty")).unwrap().len(), 0);
    assert_eq!(scratch.entries(), 1);
}

#[test]
fn traversal_devices_alternate_streams_and_staging_names_refuse_before_creation() {
    let scratch = Scratch::new();
    let root = scratch.open();
    let too_long = "a".repeat(256);
    for name in [
        "", ".", "..", "../escape", "/absolute", "a/b", "a\\b", "NUL.txt", "COM1",
        "a:stream", "a.", "a ", "a\0b", ".fr-part-claimed", &too_long,
    ] {
        assert!(matches!(
            root.begin(name, 0, ContentId::from_bytes(b"")),
            Err(Error::InvalidName)
        ));
        assert_eq!(scratch.entries(), 0);
    }
    for path in [
        r"relative",
        r"C:relative",
        r"\\server\share\never-open",
        r"\\?\GLOBALROOT\Device\never-open",
        r"\\.\pipe\never-open",
    ] {
        assert!(matches!(
            DropDirectory::open(Path::new(path), limits()),
            Err(Error::InvalidRoot)
        ));
    }
}

#[test]
fn existing_files_directories_hardlinks_and_junctions_are_never_replaced() {
    let scratch = Scratch::new();
    let outside = Scratch::new();
    fs::write(outside.0.join("original"), b"preserve outside").unwrap();
    fs::write(scratch.0.join("file"), b"preserve file").unwrap();
    fs::create_dir(scratch.0.join("directory")).unwrap();
    fs::write(scratch.0.join("directory/child"), b"preserve child").unwrap();
    fs::hard_link(outside.0.join("original"), scratch.0.join("hardlink")).unwrap();
    junction(&scratch.0.join("junction"), &outside.0);
    let root = scratch.open();
    for name in ["file", "directory", "hardlink", "junction"] {
        assert_eq!(
            verified(&root, name, b"incoming").publish(),
            Err(Error::Conflict)
        );
    }
    assert_eq!(scratch.entries(), 4, "no private staging file remains");
    assert_eq!(fs::read(scratch.0.join("file")).unwrap(), b"preserve file");
    assert_eq!(
        fs::read(scratch.0.join("directory/child")).unwrap(),
        b"preserve child"
    );
    assert_eq!(
        fs::read(scratch.0.join("hardlink")).unwrap(),
        b"preserve outside"
    );
    assert_eq!(
        fs::read(scratch.0.join("junction/original")).unwrap(),
        b"preserve outside"
    );
    let link = fr_winsys::fs::open_no_follow(
        &scratch.0.join("junction"),
        fr_winsys::fs::Open::Read,
    )
    .unwrap();
    assert!(fr_winsys::fs::inspect(&link).unwrap().reparse);
}

#[test]
fn foreign_writable_roots_and_final_or_ancestor_junctions_refuse() {
    let scratch = Scratch::new();
    let outside = Scratch::new();
    fs::create_dir(outside.0.join("child")).unwrap();
    junction(&scratch.0.join("redirect"), &outside.0);
    for path in [
        scratch.0.join("redirect"),
        scratch.0.join("redirect/child"),
    ] {
        assert!(DropDirectory::open(&path, limits()).is_err());
    }
    grant_everyone(&outside.0, "(AD)");
    assert!(matches!(
        DropDirectory::open(&outside.0, limits()),
        Err(Error::UnsafeRoot)
    ));
}

#[test]
fn selected_root_cannot_be_renamed_or_redirect_publication() {
    let scratch = Scratch::new();
    let selected = scratch.0.join("selected");
    fs::create_dir(&selected).unwrap();
    let root = DropDirectory::open(&selected, limits()).unwrap();
    let pending = verified(&root, "result", b"original root");
    assert!(fs::rename(&selected, scratch.0.join("moved")).is_err());
    assert!(!scratch.0.join("moved").exists());
    assert_eq!(pending.publish(), Ok(Publication::DurabilityUnknown));
    assert_eq!(fs::read(selected.join("result")).unwrap(), b"original root");
    drop(root);
    fs::rename(&selected, scratch.0.join("moved")).unwrap();
    assert_eq!(
        fs::read(scratch.0.join("moved/result")).unwrap(),
        b"original root"
    );
}

#[test]
fn keep_both_rechecks_authority_per_attempt_and_never_rejects_after_publication() {
    let scratch = Scratch::new();
    fs::write(scratch.0.join("report.txt"), b"existing").unwrap();
    let root = scratch.open().with_conflict_policy(ConflictPolicy::KeepBoth);
    let mut checks = 0;
    let published = verified(&root, "report.txt", b"incoming")
        .publish_named_checked(|| {
            checks += 1;
            if checks <= 2 { Ok(()) } else { Err(Error::Retired) }
        })
        .unwrap();
    assert_eq!(checks, 2);
    assert!(published.was_renamed());
    assert_eq!(published.publication(), Publication::DurabilityUnknown);
    assert_ne!(published.name(), "report.txt");
    assert_eq!(fs::read(scratch.0.join("report.txt")).unwrap(), b"existing");
    assert_eq!(fs::read(scratch.0.join(published.name())).unwrap(), b"incoming");

    let mut checks = 0;
    let refused = verified(&root, "report.txt", b"must not appear")
        .publish_named_checked(|| {
            checks += 1;
            if checks == 1 { Ok(()) } else { Err(Error::Retired) }
        });
    assert!(matches!(refused, Err(Error::Retired)));
    assert_eq!(checks, 2);
    assert_eq!(scratch.entries(), 2);
    verified(&root, "fresh", b"").cancel().unwrap();
}

#[test]
fn integrity_failure_drop_cancel_and_first_gate_refusal_reclaim_original_custody() {
    let scratch = Scratch::new();
    let root = scratch.open();
    let mut corrupt = root.begin("bad", 3, ContentId::from_bytes(b"abc")).unwrap();
    corrupt.write_chunk(0, b"abd").unwrap();
    assert_eq!(corrupt.verify(), Err(Error::Integrity));
    drop(corrupt);
    assert_eq!(scratch.entries(), 0);

    let mut partial = root.begin("partial", 3, ContentId::from_bytes(b"abc")).unwrap();
    partial.write_chunk(0, b"a").unwrap();
    partial.cancel().unwrap();
    assert_eq!(scratch.entries(), 0);

    let pending = verified(&root, "cancelled", b"verified");
    assert!(matches!(
        root.begin("over-quota", 0, ContentId::from_bytes(b"")),
        Err(Error::Quota)
    ));
    assert!(matches!(
        pending.publish_named_checked(|| Err(Error::Retired)),
        Err(Error::Retired)
    ));
    assert_eq!(scratch.entries(), 0);
    // A new transfer succeeds only after actual native cleanup and close.
    assert_eq!(
        verified(&root, "published", b"final").publish(),
        Ok(Publication::DurabilityUnknown)
    );
    assert_eq!(fs::read(scratch.0.join("published")).unwrap(), b"final");
}

#[test]
fn directory_custody_refuses_before_quota_or_staging_but_leaves_files_available() {
    let scratch = Scratch::new();
    let root = scratch.open();
    let empty = asupersync::net::atp::transport_common::flat_merkle_root_from_digests(&[]);
    let manifest = DirectoryManifest::new("tree".into(), 0, Vec::new(), empty).unwrap();
    assert!(matches!(
        root.begin_directory(manifest),
        Err(Error::Io(std::io::ErrorKind::Unsupported))
    ));
    assert_eq!(scratch.entries(), 0);
    verified(&root, "file", b"").cancel().unwrap();
}

#[path = "windows_receive/atp.rs"]
mod atp;
