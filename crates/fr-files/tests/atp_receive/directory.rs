use super::*;
const PROFILE: u16 = fr_wire::files::ATP_PORTABLE_DIRECTORY_FULL;
fn tree() -> TransferManifest {
    let mut m = manifest("a", b"abc");
    let mut digests = Vec::new();
    m.root_name = "project".into();
    m.is_directory = true;
    m.entries.clear();
    m.total_bytes = 3;
    for (index, (path, content)) in [("src/a", &b"abc"[..]), ("empty", &b""[..])]
        .iter()
        .enumerate()
    {
        let mut hash = StagedEntryReceive::new("hash-only".into());
        hash.update_with_chunk(content);
        let (digest, _, _) = hash.finalize((*path).into());
        m.entries.push(ManifestEntry {
            index: u32::try_from(index).unwrap(),
            rel_path: (*path).into(),
            size: content.len() as u64,
            sha256_hex: hex_encode(&digest.content_sha256),
            metadata: None,
            members: vec![],
        });
        digests.push(digest);
    }
    m.merkle_root_hex = flat_merkle_root_from_digests(&digests);
    m
}
fn send(receiver: &mut Receiver, bytes: &[u8]) -> Event {
    wait(
        || match receiver.push_profile(receiver.binding(), 1, bytes, PROFILE) {
            Ok(_) => true,
            Err(Error::Busy | Error::Worker(worker::Error::Busy)) => false,
            Err(e) => panic!("{e:?}"),
        },
    );
    event(receiver)
}
#[test]
fn explicitly_selected_directory_profile_publishes_and_proves_actual_file_count() {
    let f = Fixture::new();
    let (mut r, mut task) = f.spawn(Policy::conservative(), 8192);
    let begun = send(&mut r, &offered(&tree()));
    assert!(matches!(begun.outcome(), Ok(Completion::Begun(_))));
    assert_eq!(begun.profile(), PROFILE);
    assert!(matches!(
        send(&mut r, &data(0, 0, b"abc")).outcome(),
        Ok(Completion::Written(_))
    ));
    assert!(!f.root.join("project").exists());
    let result = send(&mut r, &wire(FrameType::ObjectComplete, vec![]));
    assert!(matches!(result.outcome(), Ok(Completion::Published(_))));
    let mut buffer = [0; MAX_REPLY_BYTES];
    let n = result.encode_reply(&mut buffer).unwrap().unwrap();
    let p: ReceiveReceipt = serde_json::from_slice(&parse(&buffer[..n]).payload).unwrap();
    assert_eq!(p.files, 2);
    assert_eq!(p.bytes_received, 3);
    assert!(p.committed && p.sha_ok && p.merkle_ok);
    assert_eq!(fs::read(f.root.join("project/src/a")).unwrap(), b"abc");
    stop(&mut task);
    f.input_live();
}
#[test]
fn revocation_and_idle_expiry_remove_every_directory_entry_without_publication() {
    for expire in [false, true] {
        let mut f = Fixture::new();
        let (mut r, mut task) = f.spawn(Policy::conservative(), 8192);
        send(&mut r, &offered(&tree()));
        send(&mut r, &data(0, 0, b"abc"));
        if expire {
            f.clock.advance(4_000_000_000);
        } else {
            f.input.take();
        }
        wait(|| task.is_finished());
        let _ = task.try_finish();
        f.empty();
        assert!(r.is_closed());
    }
}
#[test]
fn directory_cannot_be_smuggled_through_legacy_profile_or_change_profile_mid_transfer() {
    let f = Fixture::new();
    let (mut r, mut task) = f.spawn(Policy::conservative(), 8192);
    assert_eq!(
        r.push(r.binding(), 1, &offered(&tree())),
        Err(Error::UnsupportedProfile)
    );
    stop(&mut task);
    f.empty();
    let (mut r, mut task) = f.spawn(Policy::conservative(), 8192);
    send(&mut r, &offered(&tree()));
    assert_eq!(
        r.push(r.binding(), 1, &data(0, 0, b"abc")),
        Err(Error::UnsupportedProfile)
    );
    stop(&mut task);
    f.empty();
    f.input_live();
}
