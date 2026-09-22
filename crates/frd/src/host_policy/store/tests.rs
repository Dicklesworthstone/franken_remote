use super::*;
use crate::host_policy::options::{PolicyCommand, RunOptions};
use std::{
    fs as disk,
    os::unix::fs::{DirBuilderExt, PermissionsExt, symlink},
    sync::Barrier,
    thread,
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("fr-host-policy-{}-{id}", std::process::id()));
        disk::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn path(&self) -> PathBuf {
        self.0.join("host.json")
    }
    fn store(&self) -> Store {
        Store::new(&self.path()).unwrap()
    }
    fn write(&self, bytes: &[u8]) {
        disk::write(self.path(), bytes).unwrap();
        disk::set_permissions(self.path(), disk::Permissions::from_mode(0o600)).unwrap();
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = disk::remove_dir_all(&self.0);
    }
}
fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn missing_defaults_do_not_touch_disk_and_first_save_is_durable() {
    let root = Root::new();
    assert_eq!(root.store().load(), Ok(Policy::default()));
    assert!(!root.path().exists());
    let saved = root
        .store()
        .update(Change::Approval(Approval::None))
        .unwrap();
    assert!(saved.changed && saved.durable);
    assert_eq!(saved.policy.revision, 1);
    assert_eq!(
        disk::metadata(root.path()).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(root.store().load().unwrap(), saved.policy);
}
#[test]
fn changes_preserve_other_fields_and_repeated_saves_are_idempotent() {
    let root = Root::new();
    root.store()
        .update(Change::Approval(Approval::Local))
        .unwrap();
    let second = root
        .store()
        .update(Change::Sharing(Sharing::Tailnet))
        .unwrap();
    assert_eq!(second.policy.revision, 2);
    assert_eq!(second.policy.approval_mode, Approval::Local);
    assert_eq!(second.policy.sharing_scope, Sharing::Tailnet);
    let same = root
        .store()
        .update(Change::Sharing(Sharing::Tailnet))
        .unwrap();
    assert!(!same.changed);
    assert_eq!(same.policy, second.policy);
}
#[test]
fn startup_loads_saved_policy_and_overrides_do_not_rewrite_it() {
    let root = Root::new();
    let store = root.store();
    store.update(Change::Approval(Approval::Local)).unwrap();
    store.update(Change::Sharing(Sharing::Tailnet)).unwrap();
    let argv = args(&["--config", root.path().to_str().unwrap()]);
    let config = RunOptions::parse(&argv).unwrap().resolve().unwrap();
    assert_eq!(config.approval, Approval::Local);
    assert_eq!(config.sharing, Sharing::Tailnet);
    assert_eq!(config.port, 8443);
    let options = RunOptions {
        config: Some(root.path()),
        approval: Some(Approval::None),
        sharing: Some(Sharing::OwnUser),
        port: Some(9443),
        ..RunOptions::default()
    };
    let override_ = options.resolve().unwrap();
    assert_eq!(override_.approval, Approval::None);
    assert_eq!(override_.sharing, Sharing::OwnUser);
    assert_eq!(override_.port, 9443);
    assert_eq!(store.load().unwrap(), config.saved);
}
#[test]
fn corrupt_unknown_duplicate_and_oversize_documents_fail_closed() {
    let root = Root::new();
    for document in [
        "",
        "{}",
        "{",
        "null",
        "[]",
        r#"{"schema_version":1,"revision":1,"approval_mode":"local","sharing_scope":"everyone"}"#,
        r#"{"schema_version":2,"revision":1,"approval_mode":"local","sharing_scope":"own-user"}"#,
        r#"{"schema_version":1,"revision":0,"approval_mode":"local","sharing_scope":"own-user"}"#,
        r#"{"schema_version":1,"revision":1,"approval_mode":"local","sharing_scope":"own-user","unknown":true}"#,
        r#"{"schema_version":1,"revision":1,"approval_mode":"local","approval_mode":"none","sharing_scope":"own-user"}"#,
    ] {
        root.write(document.as_bytes());
        assert_eq!(
            root.store().load(),
            Err(Error::InvalidDocument),
            "{document}"
        );
        assert_eq!(
            root.store().update(Change::Approval(Approval::None)),
            Err(Error::InvalidDocument)
        );
        assert_eq!(disk::read(root.path()).unwrap(), document.as_bytes());
    }
    root.write(&vec![b' '; usize::try_from(MAX_BYTES).unwrap() + 1]);
    assert_eq!(root.store().load(), Err(Error::TooLarge));
}
#[test]
fn refuses_nonprivate_files_hardlinks_symlinks_and_writable_roots() {
    let root = Root::new();
    root.store()
        .update(Change::Approval(Approval::Local))
        .unwrap();
    disk::set_permissions(root.path(), disk::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(root.store().load(), Err(Error::UnsafePath));
    disk::set_permissions(root.path(), disk::Permissions::from_mode(0o600)).unwrap();
    let link = root.0.join("linked.json");
    disk::hard_link(root.path(), &link).unwrap();
    assert_eq!(root.store().load(), Err(Error::UnsafePath));
    let symbolic = root.0.join("symlink.json");
    symlink(root.path(), &symbolic).unwrap();
    let store = Store::new(&symbolic).unwrap();
    assert!(store.load().is_err());
    assert!(store.update(Change::Approval(Approval::None)).is_err());
    let directory = root.0.join("directory.json");
    disk::create_dir(&directory).unwrap();
    assert_eq!(
        Store::new(&directory).unwrap().load(),
        Err(Error::UnsafePath)
    );
    disk::set_permissions(&root.0, disk::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(root.store().load(), Err(Error::UnsafePath));
}
#[test]
fn symlink_parent_and_lock_cannot_redirect_policy_or_truncate_a_target() {
    let root = Root::new();
    let target = Root::new();
    symlink(&target.0, root.0.join("parent")).unwrap();
    let redirected = Store::new(&root.0.join("parent/host.json")).unwrap();
    assert!(redirected.load().is_err());
    assert!(redirected.update(Change::Approval(Approval::None)).is_err());
    target.write(b"untouched");
    symlink(target.path(), root.0.join(LOCK_NAME)).unwrap();
    assert!(
        root.store()
            .update(Change::Sharing(Sharing::Tailnet))
            .is_err()
    );
    assert_eq!(disk::read(target.path()).unwrap(), b"untouched");
    assert!(!root.path().exists());
}
#[test]
fn stable_lock_serializes_writers_without_blocking() {
    let root = Root::new();
    let store = root.store();
    store.update(Change::Approval(Approval::Local)).unwrap();
    let lock = File::open(root.0.join(LOCK_NAME)).unwrap();
    fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).unwrap();
    assert_eq!(
        store.update(Change::Sharing(Sharing::Tailnet)),
        Err(Error::Busy)
    );
    assert_eq!(store.load().unwrap().approval_mode, Approval::Local);
    drop(lock);
    assert!(store.update(Change::Sharing(Sharing::Tailnet)).is_ok());
}
#[test]
fn concurrent_field_updates_never_lose_each_other() {
    let root = Root::new();
    let barrier = Barrier::new(2);
    thread::scope(|scope| {
        for change in [
            Change::Approval(Approval::Local),
            Change::Sharing(Sharing::Tailnet),
        ] {
            let root = &root;
            let barrier = &barrier;
            scope.spawn(move || {
                barrier.wait();
                for _ in 0..1000 {
                    match root.store().update(change) {
                        Ok(_) => return,
                        Err(Error::Busy) => thread::sleep(std::time::Duration::from_millis(1)),
                        Err(error) => panic!("unexpected refusal {error:?}"),
                    }
                }
                panic!("writer did not make progress");
            });
        }
    });
    let policy = root.store().load().unwrap();
    assert_eq!(policy.revision, 2);
    assert_eq!(policy.approval_mode, Approval::Local);
    assert_eq!(policy.sharing_scope, Sharing::Tailnet);
}
#[test]
fn revision_exhaustion_preserves_the_original_file() {
    let root = Root::new();
    let policy = Policy {
        revision: u64::MAX,
        ..Policy::default()
    };
    let bytes = serde_json::to_vec(&policy).unwrap();
    root.write(&bytes);
    assert_eq!(
        root.store().update(Change::Approval(Approval::Local)),
        Err(Error::RevisionExhausted)
    );
    assert_eq!(disk::read(root.path()).unwrap(), bytes);
}
#[test]
fn missing_parent_is_read_only_until_an_explicit_save() {
    let root = Root::new();
    let path = root.0.join("new/private/host.json");
    let store = Store::new(&path).unwrap();
    assert_eq!(store.load().unwrap(), Policy::default());
    assert!(!root.0.join("new").exists());
    store.update(Change::Approval(Approval::Local)).unwrap();
    assert_eq!(
        disk::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
}
#[test]
fn unsafe_pathnames_are_refused_before_io() {
    for path in [
        "relative.json",
        "/",
        "/tmp/../policy.json",
        "/tmp/.frd-policy.lock",
        "/tmp/.frd-policy-pending-1",
    ] {
        assert!(Store::new(Path::new(path)).is_err(), "{path}");
    }
}
#[test]
fn startup_refuses_typos_missing_values_unknown_options_and_duplicates() {
    for argv in [
        vec!["--approval", "loacl"],
        vec!["--approval"],
        vec!["--sharing", "everyone"],
        vec!["--port", "0"],
        vec!["--port", "65536"],
        vec!["--port", "not-a-port"],
        vec!["--port", "8443", "--port", "9443"],
        vec!["--config", "--json"],
        vec!["--approval", "local", "--approval", "none"],
        vec!["--unknown", "x"],
        vec!["--json", "--json"],
        vec!["positional"],
    ] {
        assert!(RunOptions::parse(&args(&argv)).is_err(), "{argv:?}");
    }
}
#[test]
fn management_parser_requires_exact_commands_and_retains_option_values() {
    let root = Root::new();
    let path = root.path();
    let path = path.to_str().unwrap();
    let parsed =
        PolicyCommand::parse(&args(&["set", "local", "--config", path, "--json"]), true).unwrap();
    assert_eq!(parsed.path, root.path());
    assert!(matches!(
        parsed.change,
        Some(Change::Approval(Approval::Local))
    ));
    for argv in [
        vec!["set"],
        vec!["set", "local", "ignored"],
        vec!["get", "extra"],
        vec!["--config", path, "--config", path],
        vec!["--unknown"],
        vec!["--json", "--json"],
    ] {
        assert!(PolicyCommand::parse(&args(&argv), true).is_err());
    }
}

#[test]
fn private_leaf_does_not_make_an_untrusted_ancestor_safe() {
    let root = Root::new();
    let path = root.0.join("private/host.json");
    let store = Store::new(&path).unwrap();
    store.update(Change::Approval(Approval::Local)).unwrap();
    disk::set_permissions(&root.0, disk::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(store.load(), Err(Error::UnsafePath));
    assert_eq!(
        store.update(Change::Approval(Approval::None)),
        Err(Error::UnsafePath)
    );
}
