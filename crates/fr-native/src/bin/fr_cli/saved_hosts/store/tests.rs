use super::*;
use std::os::unix::fs::{PermissionsExt, symlink};
fn root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "fr-saved-hosts-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    path
}
fn peer(alias: &str) -> SavedPeer {
    SavedPeer::new(
        alias.into(),
        format!("n-{alias}"),
        format!("{alias}.fixture.ts.net"),
        8443,
    )
    .unwrap()
}
fn private_file(path: &Path, bytes: &[u8]) {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
}
#[test]
fn empty_read_creates_nothing_and_saves_survive_reopening() {
    let path = root().join("missing/app/hosts.json");
    let store = Store::new(&path).unwrap();
    assert_eq!(store.load().unwrap().list(), []);
    assert!(!path.parent().unwrap().exists());
    assert!(store.update(Change::Add(peer("office"))).unwrap().durable);
    let loaded = Store::new(&path).unwrap().load().unwrap();
    assert_eq!(loaded.get("office"), Some(&peer("office")));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    store.update(Change::Forget("office".into())).unwrap();
    assert_eq!(Store::new(&path).unwrap().load().unwrap().list(), []);
}
#[test]
fn publication_replaces_an_inode_without_truncating_existing_readers() {
    let path = root().join("hosts.json");
    let store = Store::new(&path).unwrap();
    store.update(Change::Add(peer("office"))).unwrap();
    let mut original = File::open(&path).unwrap();
    store.update(Change::Add(peer("home"))).unwrap();
    let mut old = Vec::new();
    original.read_to_end(&mut old).unwrap();
    assert_eq!(
        SavedPeers::from_json(&old).unwrap().list(),
        &[peer("office")]
    );
    assert_eq!(store.load().unwrap().list().len(), 2);
}
#[test]
fn duplicate_and_malformed_documents_do_not_replace_existing_bytes() {
    let path = root().join("hosts.json");
    let store = Store::new(&path).unwrap();
    store.update(Change::Add(peer("office"))).unwrap();
    let before = std::fs::read(&path).unwrap();
    assert!(matches!(
        store.update(Change::Add(peer("office"))),
        Err(Error::Data(
            fr_client::discovery::saved_peers::SavedPeerError::Duplicate
        ))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let bad = root().join("hosts.json");
    private_file(&bad, b"{invalid document, secret payload}");
    let store = Store::new(&bad).unwrap();
    assert!(matches!(
        store.update(Change::Add(peer("office"))),
        Err(Error::Data(_))
    ));
    assert_eq!(
        std::fs::read(&bad).unwrap(),
        b"{invalid document, secret payload}"
    );
}
#[test]
fn stable_writer_lock_refuses_concurrent_updates_then_releases() {
    let path = root().join("hosts.json");
    let store = Store::new(&path).unwrap();
    store.update(Change::Add(peer("office"))).unwrap();
    let dir = store.directory(false).unwrap().unwrap();
    let lock = WriterLock::acquire(&dir).unwrap();
    assert!(matches!(
        store.update(Change::Add(peer("home"))),
        Err(Error::Busy)
    ));
    assert_eq!(store.load().unwrap().list().len(), 1);
    drop(lock);
    store.update(Change::Add(peer("home"))).unwrap();
    assert_eq!(store.load().unwrap().list().len(), 2);
}
#[test]
fn symlinks_hardlinks_and_nonregular_files_are_refused() {
    let root = root();
    let outside = root.join("original");
    private_file(&outside, b"private original");
    let link = root.join("symlink");
    symlink(&outside, &link).unwrap();
    assert_eq!(Store::new(&link).unwrap().load(), Err(Error::UnsafePath));
    assert!(
        Store::new(&link)
            .unwrap()
            .update(Change::Add(peer("office")))
            .is_err()
    );
    assert_eq!(std::fs::read(&outside).unwrap(), b"private original");
    let hard = root.join("hardlink");
    std::fs::hard_link(&outside, &hard).unwrap();
    assert_eq!(Store::new(&hard).unwrap().load(), Err(Error::UnsafePath));
    let directory = root.join("directory");
    std::fs::create_dir(&directory).unwrap();
    assert_eq!(
        Store::new(&directory).unwrap().load(),
        Err(Error::UnsafePath)
    );
    let parent_link = root.join("parent-link");
    symlink(&directory, &parent_link).unwrap();
    assert!(
        Store::new(&parent_link.join("hosts.json"))
            .unwrap()
            .load()
            .is_err()
    );
}
#[test]
fn permissions_size_and_path_bounds_fail_before_document_use() {
    let root = root();
    let exposed = root.join("exposed");
    private_file(&exposed, b"{}");
    std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(Store::new(&exposed).unwrap().load(), Err(Error::UnsafePath));
    let large = root.join("large");
    private_file(&large, &vec![b' '; MAX_SAVED_PEER_BYTES + 1]);
    assert_eq!(Store::new(&large).unwrap().load(), Err(Error::TooLarge));
    let writable = root.join("writable");
    std::fs::create_dir(&writable).unwrap();
    std::fs::set_permissions(&writable, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        Store::new(&writable.join("hosts.json"))
            .unwrap()
            .update(Change::Add(peer("office"))),
        Err(Error::UnsafePath)
    ));
    for path in [
        Path::new("relative"),
        Path::new("/tmp/../hosts.json"),
        Path::new("/tmp/.fr-saved-hosts.lock"),
    ] {
        assert!(Store::new(path).is_err());
    }
}
