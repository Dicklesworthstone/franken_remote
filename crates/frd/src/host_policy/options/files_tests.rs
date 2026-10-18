//! Real parser -> production startup helper -> pinned receive configuration ->
//! real filesystem publication. No runtime, worker, network or service activation.
use super::*;
use asupersync::{
    atp::object::ContentId,
    net::atp::{
        transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
        transport_tcp::ManifestEntry,
    },
};
use fr_files::receive::{DirectoryManifest, Error as ReceiveError, Publication};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

fn parse(args: &[&str]) -> Result<RunOptions, Error> {
    RunOptions::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "fr-host-files-policy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn options(path: &Path, policy: Option<&str>) -> RunOptions {
    let mut args = vec!["--files", path.to_str().unwrap()];
    if let Some(policy) = policy {
        args.extend(["--files-conflict", policy]);
    }
    parse(&args).unwrap()
}
fn tree() -> DirectoryManifest {
    let mut hash = StagedEntryReceive::new("hash-only".into());
    hash.update_with_chunk(b"new");
    let (digest, _, _) = hash.finalize("src/content".into());
    DirectoryManifest::new(
        "project".into(),
        3,
        vec![ManifestEntry {
            index: 0,
            rel_path: "src/content".into(),
            size: 3,
            sha256_hex: hex_encode(&digest.content_sha256),
            metadata: None,
            members: vec![],
        }],
        flat_merkle_root_from_digests(&[digest]),
    )
    .unwrap()
}

#[test]
fn conflict_policy_defaults_to_reject_and_enables_no_other_capability() {
    let off = parse(&[]).unwrap();
    assert_eq!(off.file_conflict_policy().unwrap(), ConflictPolicy::Reject);
    assert!(off.open_files().unwrap().is_none());
    for (name, expected) in [
        ("reject", ConflictPolicy::Reject),
        ("keep-both", ConflictPolicy::KeepBoth),
    ] {
        let run = options(Path::new("/not-opened-by-parsing"), Some(name));
        assert_eq!(run.files_conflict, Some(expected));
        assert_eq!(run.file_conflict_policy().unwrap(), expected);
        assert!(run.input_agent.is_none());
        assert!(!run.clipboard && !run.audio);
        assert!(run.approval.is_none() && run.sharing.is_none());
    }
}

#[test]
fn missing_duplicate_or_unsafe_policy_values_refuse_before_directory_io() {
    for args in [
        vec!["--files-conflict", "keep-both"],
        vec!["--files", "/drop", "--files-conflict"],
        vec!["--files", "/drop", "--files-conflict", "--once"],
        vec!["--files", "/drop", "--files-conflict", ""],
        vec!["--files", "/drop", "--files-conflict", "replace"],
        vec!["--files", "/drop", "--files-conflict", "overwrite"],
        vec!["--files", "/drop", "--files-conflict", "merge"],
        vec!["--files", "/drop", "--files-conflict", "KeepBoth"],
        vec!["--files", "/drop", "--files-conflict", "keep-both "],
        vec!["--files", "/drop", "--files-conflict=keep-both"],
        vec![
            "--files",
            "/drop",
            "--files-conflict",
            "reject",
            "--files-conflict",
            "keep-both",
        ],
        vec![
            "--files",
            "/drop",
            "--files-conflict",
            "keep-both",
            "--files-conflict",
            "keep-both",
        ],
    ] {
        assert!(
            matches!(parse(&args), Err(Error::InvalidArgument)),
            "{args:?}"
        );
    }
    assert!(parse(&["--files-conflict", "keep-both", "--files", "/drop"]).is_ok());
}

#[test]
fn programmatic_orphans_refuse_without_loading_saved_policy_or_opening_a_root() {
    let mut run = RunOptions {
        files_conflict: Some(ConflictPolicy::KeepBoth),
        config: Some(PathBuf::from("relative-invalid-policy")),
        ..RunOptions::default()
    };
    for kind in 0..3 {
        run.files_conflict = (kind == 0).then_some(ConflictPolicy::KeepBoth);
        run.files_max_file_bytes = (kind == 1).then_some(3);
        run.files_max_session_bytes = (kind == 2).then_some(3);
        assert_eq!(run.file_conflict_policy(), Err(Error::InvalidArgument));
        assert!(matches!(run.resolve(), Err(Error::InvalidArgument)));
        assert_eq!(run.open_files().unwrap_err(), DirectoryRefusal::Limits);
    }
}

#[test]
fn selected_policy_reaches_actual_file_and_whole_tree_publication() {
    for policy in [None, Some("reject"), Some("keep-both")] {
        let scratch = Scratch::new();
        fs::write(scratch.0.join("document"), b"original").unwrap();
        fs::create_dir(scratch.0.join("project")).unwrap();
        fs::write(scratch.0.join("project/precious"), b"original tree").unwrap();
        let directory = options(&scratch.0, policy).open_files().unwrap().unwrap();
        let config = directory.configuration();
        let mut file = config
            .directory
            .begin("document", 3, ContentId::from_bytes(b"new"))
            .unwrap();
        file.write_chunk(0, b"new").unwrap();
        file.verify().unwrap();
        let file = file.publish_named();
        let mut folder = config.directory.begin_directory(tree()).unwrap();
        folder.write_entry(0, 0, b"new").unwrap();
        folder.verify().unwrap();
        let folder = folder.publish_named();
        if policy == Some("keep-both") {
            let file = file.unwrap();
            let folder = folder.unwrap();
            assert!(file.was_renamed() && folder.was_renamed());
            assert_eq!(file.publication(), Publication::Durable);
            assert_eq!(folder.publication(), Publication::Durable);
            assert_eq!(fs::read(scratch.0.join(file.name())).unwrap(), b"new");
            assert_eq!(
                fs::read(scratch.0.join(folder.name()).join("src/content")).unwrap(),
                b"new"
            );
            assert!(!scratch.0.join(folder.name()).join("precious").exists());
            assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 4);
        } else {
            assert_eq!(file.unwrap_err(), ReceiveError::Conflict);
            assert_eq!(folder.unwrap_err(), ReceiveError::Conflict);
            assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 2);
        }
        assert_eq!(fs::read(scratch.0.join("document")).unwrap(), b"original");
        assert_eq!(
            fs::read(scratch.0.join("project/precious")).unwrap(),
            b"original tree"
        );
    }
}

#[test]
fn configured_byte_budgets_and_clone_reservations_survive_policy_selection() {
    let scratch = Scratch::new();
    let mut run = options(&scratch.0, Some("keep-both"));
    run.files_max_file_bytes = Some(3);
    run.files_max_session_bytes = Some(6);
    let keep = run.open_files().unwrap().unwrap();
    let reject = keep.clone().with_conflict_policy(ConflictPolicy::Reject);
    assert_eq!(keep.limits(), reject.limits());
    assert_eq!(keep.limits().max_file_bytes, 3);
    let config = keep.configuration();
    assert_eq!(config.policy.max_session_bytes, 6);
    let held = config.directory.begin_directory(tree()).unwrap();
    let rejected_config = reject.configuration();
    assert!(matches!(
        rejected_config
            .directory
            .begin("file", 0, ContentId::from_bytes(b"")),
        Err(ReceiveError::Quota)
    ));
    held.cancel().unwrap();
    assert!(matches!(
        config
            .directory
            .begin("file", 4, ContentId::from_bytes(b"four")),
        Err(ReceiveError::Quota)
    ));
    config
        .directory
        .begin("file", 3, ContentId::from_bytes(b"new"))
        .unwrap()
        .cancel()
        .unwrap();
    assert_eq!(fs::read_dir(&scratch.0).unwrap().count(), 0);
    run.files_max_session_bytes = Some(2);
    assert_eq!(run.open_files().unwrap_err(), DirectoryRefusal::Limits);
}

#[test]
fn configured_keep_both_keeps_original_directory_identity_and_integrity_checks() {
    let scratch = Scratch::new();
    let selected = scratch.0.join("selected");
    let moved = scratch.0.join("moved");
    fs::create_dir(&selected).unwrap();
    fs::set_permissions(&selected, fs::Permissions::from_mode(0o700)).unwrap();
    let directory = options(&selected, Some("keep-both"))
        .open_files()
        .unwrap()
        .unwrap();
    fs::rename(&selected, &moved).unwrap();
    fs::create_dir(&selected).unwrap();
    fs::write(moved.join("document"), b"original").unwrap();
    let config = directory.configuration();
    let mut file = config
        .directory
        .begin("document", 3, ContentId::from_bytes(b"new"))
        .unwrap();
    file.write_chunk(0, b"bad").unwrap();
    assert_eq!(file.verify(), Err(ReceiveError::Integrity));
    assert_eq!(file.publish_named().unwrap_err(), ReceiveError::Retired);
    let mut file = config
        .directory
        .begin("document", 3, ContentId::from_bytes(b"new"))
        .unwrap();
    file.write_chunk(0, b"new").unwrap();
    file.verify().unwrap();
    let published = file.publish_named().unwrap();
    assert!(published.was_renamed());
    assert_eq!(fs::read(moved.join(published.name())).unwrap(), b"new");
    assert_eq!(fs::read(moved.join("document")).unwrap(), b"original");
    assert_eq!(fs::read_dir(&selected).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&moved).unwrap().count(), 2);
}

#[test]
fn conflict_preference_never_relaxes_drop_directory_safety() {
    let scratch = Scratch::new();
    let link = scratch.0.join("link");
    symlink(&scratch.0, &link).unwrap();
    for policy in [None, Some("reject"), Some("keep-both")] {
        assert_eq!(
            options(&link, policy).open_files().unwrap_err(),
            DirectoryRefusal::Symlink
        );
        assert_eq!(
            options(Path::new("relative"), policy)
                .open_files()
                .unwrap_err(),
            DirectoryRefusal::Relative
        );
        assert_eq!(
            options(&scratch.0.join("missing"), policy)
                .open_files()
                .unwrap_err(),
            DirectoryRefusal::Missing
        );
    }
}
