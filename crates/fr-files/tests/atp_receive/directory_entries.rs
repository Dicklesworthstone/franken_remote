//! Pinned ATP frames through the actual disk worker and original authority.
//! The local clock and controller are explicit fixtures; no network claim.
use super::*;
use asupersync::net::atp::transport_common::{EntryMetadata, FileKind, metadata_commitment};

const PROFILE: u16 = fr_wire::files::ATP_PORTABLE_DIRECTORY_ENTRIES;

fn commit_metadata(manifest: &mut TransferManifest) {
    let regular = EntryMetadata::default();
    let pairs: Vec<_> = manifest
        .entries
        .iter()
        .map(|entry| {
            (
                entry.rel_path.as_str(),
                entry.metadata.as_ref().unwrap_or(&regular),
            )
        })
        .collect();
    manifest.metadata_root_hex = metadata_commitment(&pairs);
}

fn tree(all_empty: bool) -> TransferManifest {
    let mut tree = manifest("base", b"");
    tree.root_name = "project".into();
    tree.is_directory = true;
    tree.entries.clear();
    let items: Vec<(&str, Option<&[u8]>)> = if all_empty {
        vec![("empty-sibling", None), ("src/deep/empty-leaf", None)]
    } else {
        vec![
            ("empty-sibling", None),
            ("src/data", Some(b"abc")),
            ("src/deep/empty-leaf", None),
            ("zero-file", Some(b"")),
        ]
    };
    let mut digests = Vec::new();
    for (index, (path, content)) in items.iter().enumerate() {
        let bytes = content.unwrap_or_default();
        let mut hash = StagedEntryReceive::new("hash-only".into());
        hash.update_with_chunk(bytes);
        let (digest, _, _) = hash.finalize((*path).into());
        tree.entries.push(ManifestEntry {
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
    tree.total_bytes = tree.entries.iter().map(|entry| entry.size).sum();
    tree.merkle_root_hex = flat_merkle_root_from_digests(&digests);
    commit_metadata(&mut tree);
    tree
}

fn send(receiver: &mut Receiver, frame: &[u8]) -> Event {
    wait(
        || match receiver.push_profile(receiver.binding(), 1, frame, PROFILE) {
            Ok(_) => true,
            Err(Error::Busy | Error::Worker(worker::Error::Busy)) => false,
            Err(error) => panic!("structural ATP frame refused: {error:?}"),
        },
    );
    event(receiver)
}

#[test]
fn profile_three_publishes_mixed_and_all_empty_trees_and_proves_every_entry() {
    for all_empty in [false, true] {
        let f = Fixture::new();
        let (mut receiver, mut task) = f.spawn(Policy::conservative(), 8192);
        let manifest = tree(all_empty);
        let begun = send(&mut receiver, &offered(&manifest));
        assert!(matches!(begun.outcome(), Ok(Completion::Begun(_))));
        assert_eq!(begun.profile(), PROFILE);
        if !all_empty {
            assert!(matches!(
                send(&mut receiver, &data(1, 0, b"abc")).outcome(),
                Ok(Completion::Written(_))
            ));
        }
        assert!(!f.root.join("project").exists());
        let result = send(&mut receiver, &wire(FrameType::ObjectComplete, vec![]));
        let Ok(Completion::Published(receipt)) = result.outcome() else {
            panic!("tree not published");
        };
        assert_eq!(receipt.publication, Publication::Durable);
        let mut buffer = [0; MAX_REPLY_BYTES];
        let length = result.encode_reply(&mut buffer).unwrap().unwrap();
        let proof: ReceiveReceipt =
            serde_json::from_slice(&parse(&buffer[..length]).payload).unwrap();
        assert_eq!(proof.files as usize, manifest.entries.len());
        assert_eq!(proof.bytes_received, manifest.total_bytes);
        assert!(proof.committed && proof.sha_ok && proof.merkle_ok);
        assert!(proof.committed_paths.is_empty());
        for path in ["empty-sibling", "src/deep/empty-leaf"] {
            assert!(f.root.join("project").join(path).is_dir());
            assert_eq!(
                fs::read_dir(f.root.join("project").join(path))
                    .unwrap()
                    .count(),
                0
            );
        }
        if !all_empty {
            assert_eq!(fs::read(f.root.join("project/src/data")).unwrap(), b"abc");
            assert!(f.root.join("project/zero-file").is_file());
        }
        assert!(!format!("{receiver:?} {result:?}").contains("project"));
        assert!(!String::from_utf8_lossy(&buffer[..length]).contains("empty-sibling"));
        stop(&mut task);
        f.input_live();
    }
}

#[test]
fn directory_data_is_terminal_for_files_and_preserves_the_original_input_owner() {
    for bytes in [&b""[..], &b"x"[..]] {
        let f = Fixture::new();
        fs::write(f.root.join("previous"), b"already published").unwrap();
        let (mut receiver, mut task) = f.spawn(Policy::conservative(), 8192);
        send(&mut receiver, &offered(&tree(true)));
        assert_eq!(
            receiver.push_profile(receiver.binding(), 1, &data(0, 0, bytes), PROFILE),
            Err(Error::ResourceLimit)
        );
        assert!(receiver.is_closed());
        stop(&mut task);
        assert!(!f.root.join("project").exists());
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1);
        assert_eq!(
            fs::read(f.root.join("previous")).unwrap(),
            b"already published"
        );
        f.input_live();
    }
}

#[test]
fn missing_stale_or_unsupported_metadata_refuses_before_any_staging() {
    for mutation in 0..7 {
        let f = Fixture::new();
        let (mut receiver, mut task) = f.spawn(Policy::conservative(), 8192);
        let mut manifest = tree(false);
        match mutation {
            0 => manifest.metadata_root_hex = None,
            1 => manifest.metadata_root_hex = Some("00".repeat(32)),
            2 => manifest.entries[0].metadata = None,
            3 => manifest.entries[1].rel_path = "different-regular-file".into(),
            4 => {
                manifest.entries[1].metadata = Some(EntryMetadata::default());
                commit_metadata(&mut manifest);
            }
            5 => {
                manifest.entries[0].metadata.as_mut().unwrap().unix_mode = Some(0o777);
                commit_metadata(&mut manifest);
            }
            _ => manifest.entries[0].sha256_hex = "00".repeat(32),
        }
        let expected = if mutation == 0 {
            Error::UnsupportedProfile
        } else {
            Error::Manifest
        };
        assert_eq!(
            receiver.push_profile(receiver.binding(), 1, &offered(&manifest), PROFILE),
            Err(expected)
        );
        stop(&mut task);
        f.empty();
        f.input_live();
    }
}

#[test]
fn profile_two_stays_content_only_and_an_active_tree_cannot_change_profile() {
    for mutation in 0..4 {
        let f = Fixture::new();
        let (mut receiver, mut task) = f.spawn(Policy::conservative(), 8192);
        let mut manifest = tree(true);
        let expected = match mutation {
            0 => Error::UnsupportedProfile,
            1 => {
                manifest.metadata_root_hex = None;
                Error::Manifest
            }
            2 => {
                send(&mut receiver, &offered(&manifest));
                Error::UnsupportedProfile
            }
            _ => {
                manifest.metadata_root_hex = None;
                for entry in &mut manifest.entries {
                    entry.metadata = None;
                }
                Error::UnsupportedProfile
            }
        };
        let bytes = if mutation == 2 {
            wire(FrameType::ObjectComplete, vec![])
        } else {
            offered(&manifest)
        };
        let profile = if mutation == 3 {
            PROFILE
        } else {
            fr_wire::files::ATP_PORTABLE_DIRECTORY_FULL
        };
        assert_eq!(
            receiver.push_profile(receiver.binding(), 1, &bytes, profile),
            Err(expected)
        );
        stop(&mut task);
        f.empty();
        f.input_live();
    }
}

#[test]
fn structural_staging_obeys_cancellation_revocation_and_original_expiry() {
    for fault in 0..3 {
        let mut f = Fixture::new();
        fs::write(f.root.join("previous"), b"keep").unwrap();
        let (mut receiver, mut task) = f.spawn(Policy::conservative(), 8192);
        send(&mut receiver, &offered(&tree(false)));
        send(&mut receiver, &data(1, 0, b"a"));
        match fault {
            0 => receiver.cancel(receiver.binding(), 1).unwrap(),
            1 => {
                f.input.take();
            }
            _ => {
                f.clock.advance(4_000_000_000);
            }
        }
        wait(|| task.is_finished());
        assert_eq!(task.try_finish_cleanup(), Some(Ok(())));
        assert!(receiver.is_closed());
        assert_eq!(fs::read_dir(&f.root).unwrap().count(), 1);
        assert_eq!(fs::read(f.root.join("previous")).unwrap(), b"keep");
        assert!(!f.root.join("project").exists());
        if fault == 0 {
            f.input_live();
        }
    }
}

#[test]
fn valid_structural_metadata_never_substitutes_for_the_content_root() {
    for all_empty in [false, true] {
        let f = Fixture::new();
        let (mut receiver, mut task) = f.spawn(Policy::conservative(), 8192);
        let mut manifest = tree(all_empty);
        manifest.merkle_root_hex = "00".repeat(32);
        send(&mut receiver, &offered(&manifest));
        if !all_empty {
            send(&mut receiver, &data(1, 0, b"abc"));
        }
        let result = send(&mut receiver, &wire(FrameType::ObjectComplete, vec![]));
        assert_eq!(
            result.outcome(),
            Err(worker::Error::Session(session::Error::Storage(
                receive::Error::Integrity
            )))
        );
        assert!(
            result
                .encode_reply(&mut [0; MAX_REPLY_BYTES])
                .unwrap()
                .is_none()
        );
        stop(&mut task);
        f.empty();
        f.input_live();
    }
}

#[test]
fn unowned_content_inside_an_empty_leaf_retains_cleanup_failure_and_quota() {
    let f = Fixture::new();
    let (mut receiver, mut task) = f.spawn(Policy::conservative(), 8192);
    send(&mut receiver, &offered(&tree(true)));
    let staging = fs::read_dir(&f.root)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let unowned = staging.join("src/deep/empty-leaf/local-only");
    fs::write(&unowned, b"local content must survive").unwrap();
    task.stop();
    wait(|| task.is_finished());
    assert_eq!(task.try_finish_cleanup(), Some(Err(worker::Error::Cleanup)));
    assert_eq!(task.try_finish_cleanup(), Some(Err(worker::Error::Cleanup)));
    assert_eq!(fs::read(&unowned).unwrap(), b"local content must survive");
    assert!(!staging.join("empty-sibling").exists());
    assert!(!f.root.join("project").exists());
    assert!(matches!(
        f.directory.begin("second", 0, ContentId::from_bytes(b"")),
        Err(receive::Error::Quota)
    ));
    f.input_live();
}
