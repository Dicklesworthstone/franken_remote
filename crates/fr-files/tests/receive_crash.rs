//! Actual Linux publication and process death, not a transport/worker IPC test.
//! tmpfs and procfs are required here to exercise unnamed staging explicitly.
#![cfg(target_os = "linux")]
use asupersync::{
    atp::object::ContentId,
    net::atp::transport_common::{StagedEntryReceive, flat_merkle_root_from_digests, hex_encode},
};
use fr_files::receive::{DropDirectory, Error, Limits, MAX_CHUNK_BYTES, PendingFile, Publication};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const ROOT: &str = "FR_RECEIVE_CRASH_ROOT";
const PHASE: &str = "FR_RECEIVE_CRASH_PHASE";
const BYTES: usize = 3 * MAX_CHUNK_BYTES + 17;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = PathBuf::from("/dev/shm").join(format!(
            "fr-receive-crash-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join(".fr-part-legacy"), b"legacy staging: not ours").unwrap();
        fs::write(path.join("existing"), b"existing user content").unwrap();
        Self(path)
    }
    fn names(&self) -> Vec<String> {
        let mut names = fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        names
    }
    fn untouched(&self) {
        assert_eq!(self.names(), [".fr-part-legacy", "existing"]);
        assert_eq!(
            fs::read(self.0.join(".fr-part-legacy")).unwrap(),
            b"legacy staging: not ours"
        );
        assert_eq!(
            fs::read(self.0.join("existing")).unwrap(),
            b"existing user content"
        );
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn open(path: &Path) -> DropDirectory {
    DropDirectory::open(
        path,
        Limits {
            max_file_bytes: BYTES as u64,
            max_reserved_bytes: BYTES as u64,
            max_transfers: 1,
        },
    )
    .unwrap()
}
fn data() -> Vec<u8> {
    (0..BYTES).map(|i| u8::try_from(i % 251).unwrap()).collect()
}
fn append(pending: &mut PendingFile, data: &[u8]) {
    for chunk in data.chunks(MAX_CHUNK_BYTES) {
        pending
            .write_chunk(pending.received_bytes(), chunk)
            .unwrap();
    }
}

#[test]
fn publication_crash_child() {
    let Some(path) = std::env::var_os(ROOT) else {
        return;
    };
    let phase = std::env::var(PHASE).unwrap();
    let root = open(Path::new(&path));
    let bytes = data();
    // Use the actual ATP manifest integrity contract, not only ContentId.
    let mut hash = StagedEntryReceive::new("local-fixture".into());
    hash.update_with_chunk(&bytes);
    let (digest, _, _) = hash.finalize("complete".into());
    let expected = hex_encode(&digest.content_sha256);
    let merkle = flat_merkle_root_from_digests(&[digest]);
    let mut pending = root
        .begin_manifest("complete", BYTES as u64, expected, merkle)
        .unwrap();
    assert_eq!(
        root.begin("blocked", 0, ContentId::from_bytes(b""))
            .unwrap_err(),
        Error::Quota
    );
    if phase == "receiving" {
        pending.write_chunk(0, &bytes[..MAX_CHUNK_BYTES]).unwrap();
    } else {
        let mut incoming = bytes;
        if phase == "corrupt" {
            incoming[0] ^= 1;
        }
        append(&mut pending, &incoming);
        if phase == "corrupt" {
            assert_eq!(pending.verify(), Err(Error::Integrity));
            assert_eq!(pending.verify(), Err(Error::Retired));
        } else {
            pending.verify().unwrap();
        }
    }
    let held = if phase == "published" {
        assert_eq!(pending.publish(), Ok(Publication::Durable));
        None
    } else {
        assert!(matches!(
            phase.as_str(),
            "receiving" | "verified" | "corrupt"
        ));
        Some(pending)
    };
    // Keep the pending owner alive. SIGKILL, not a scope exit, ends this state.
    println!("PUBLICATION_PHASE_READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
        std::hint::black_box(&held);
    }
}

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn kill_at(root: &Scratch, phase: &str) {
    let mut child = KillOnDrop(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "publication_crash_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROOT, &root.0)
            .env(PHASE, phase)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let output = child.0.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let ready = BufReader::new(output)
            .lines()
            .take(10)
            .any(|line| line.is_ok_and(|line| line.contains("PUBLICATION_PHASE_READY")));
        let _ = send.send(ready);
    });
    let ready = receive.recv_timeout(Duration::from_secs(5));
    // Collect names before the kill too: even fully verified staging is hidden.
    let names = root.names();
    let killed = child.0.kill();
    let status = child.0.wait().unwrap();
    reader.join().unwrap();
    assert_eq!(
        ready,
        Ok(true),
        "child must actually reach the requested phase"
    );
    killed.unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "must execute SIGKILL, not graceful Drop"
    );
    let expected = if phase == "published" {
        vec![".fr-part-legacy", "complete", "existing"]
    } else {
        vec![".fr-part-legacy", "existing"]
    };
    assert_eq!(
        names, expected,
        "no visible partial staging before process death"
    );
    assert_eq!(root.names(), expected, "no orphan after process death");
}

#[test]
fn receiving_crash_drops_only_its_unpublished_inode() {
    let scratch = Scratch::new();
    kill_at(&scratch, "receiving");
    scratch.untouched();
}
#[test]
fn verified_crash_never_publishes_without_the_final_gate() {
    let scratch = Scratch::new();
    kill_at(&scratch, "verified");
    scratch.untouched();
    // A new local operation starts from byte zero and verifies its own content;
    // opening the directory does not resurrect the killed owner's transaction.
    let root = open(&scratch.0);
    let mut new = root
        .begin("complete", 3, ContentId::from_bytes(b"new"))
        .unwrap();
    assert_eq!(new.received_bytes(), 0);
    new.write_chunk(0, b"new").unwrap();
    new.verify().unwrap();
    assert_eq!(new.publish(), Ok(Publication::Durable));
    assert_eq!(fs::read(scratch.0.join("complete")).unwrap(), b"new");
}
#[test]
fn integrity_failure_stays_unpublished_even_without_cleanup() {
    let scratch = Scratch::new();
    kill_at(&scratch, "corrupt");
    scratch.untouched();
}
#[test]
fn published_file_survives_its_process_being_killed() {
    let scratch = Scratch::new();
    kill_at(&scratch, "published");
    assert_eq!(fs::read(scratch.0.join("complete")).unwrap(), data());
    assert_eq!(
        fs::metadata(scratch.0.join("complete"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::read(scratch.0.join("existing")).unwrap(),
        b"existing user content"
    );
    assert_eq!(
        fs::read(scratch.0.join(".fr-part-legacy")).unwrap(),
        b"legacy staging: not ours"
    );
}
#[test]
fn another_live_transfer_and_old_entries_survive_a_crash() {
    let scratch = Scratch::new();
    let root = open(&scratch.0);
    let mut live = root
        .begin("live", 4, ContentId::from_bytes(b"live"))
        .unwrap();
    live.write_chunk(0, b"li").unwrap();
    kill_at(&scratch, "verified");
    scratch.untouched();
    assert_eq!(
        root.begin("blocked", 0, ContentId::from_bytes(b""))
            .unwrap_err(),
        Error::Quota
    );
    live.write_chunk(2, b"ve").unwrap();
    live.verify().unwrap();
    assert_eq!(live.publish(), Ok(Publication::Durable));
    assert_eq!(fs::read(scratch.0.join("live")).unwrap(), b"live");
}
#[test]
fn repeated_crashes_do_not_accumulate_partial_files() {
    let scratch = Scratch::new();
    for _ in 0..4 {
        for phase in ["receiving", "verified", "corrupt"] {
            kill_at(&scratch, phase);
            scratch.untouched();
        }
    }
}
