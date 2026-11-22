use super::*;
use std::{
    io::{BufRead, Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

fn directory() -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    // Separate PID namespaces can share /tmp. Never remove a prior fixture,
    // and atomically retry a collision instead of assuming the PID is unique.
    for _ in 0..32 {
        let path = std::env::temp_dir().join(format!(
            "fr-hi-{}-{stamp:x}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::DirBuilder::new().mode(0o700).create(&path) {
            Ok(()) => return path,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("creating test directory: {error}"),
        }
    }
    panic!("could not create an unused test directory");
}

fn owner(path: &Path) -> u32 {
    fs::metadata(path).unwrap().uid()
}

fn child(path: &Path, mode: &str) -> Child {
    child_command(path, mode).spawn().unwrap()
}

fn child_command(path: &Path, mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .arg("instance_lock_child_process")
        .arg("--nocapture")
        .env("FR_HELPER_LOCK_TEST_PATH", path)
        .env("FR_HELPER_LOCK_TEST_MODE", mode)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn announce_and_wait() {
    println!("FR_HELPER_INSTANCE_HELD");
    std::io::stdout().flush().unwrap();
    // EOF ends this child if its parent aborts, so a failed assertion cannot
    // strand a waiting process. A byte can release it without killing it.
    let _ = std::io::stdin().read(&mut [0_u8; 1]);
}

fn wait_for_custody(process: &mut Child) {
    let stdout = process.stdout.take().unwrap();
    let mut output = std::io::BufReader::new(stdout);
    for _ in 0..16 {
        let mut line = String::new();
        if output.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line.contains("FR_HELPER_INSTANCE_HELD") {
            process.stdout = Some(output.into_inner());
            return;
        }
    }
    panic!("the child never acquired its namespace lock");
}

/// Invoked independently so process locks, inheritance and crash release are
/// tested against the shipping lock implementation, not an in-memory model.
#[test]
fn instance_lock_child_process() {
    let Some(path) = std::env::var_os("FR_HELPER_LOCK_TEST_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let uid = owner(path.parent().unwrap());
    match std::env::var("FR_HELPER_LOCK_TEST_MODE").unwrap().as_str() {
        "blocked" => assert_eq!(lock_file(&path, uid).unwrap_err(), Error::InUse),
        "acquire" => {
            let _lock = lock_file(&path, uid).unwrap();
        }
        "hold" => {
            let _lock = lock_file(&path, uid).unwrap();
            announce_and_wait();
        }
        #[cfg(target_os = "linux")]
        "inherited-parent" => inherited_children_outlive_cancelled_owner(&path, uid),
        #[cfg(target_os = "linux")]
        "inherited" => {
            // Inspect the actual inherited descriptor after exec, rather
            // than opening another description of the same pathname.
            let fd = std::env::var("FR_HELPER_LOCK_TEST_FD").unwrap();
            let inherited = fs::metadata(format!("/proc/self/fd/{fd}")).unwrap();
            assert_eq!(
                Identity::of(&inherited),
                Identity::of(&fs::metadata(&path).unwrap())
            );
            announce_and_wait();
        }
        mode => panic!("unexpected child mode {mode}"),
    }
}

#[test]
fn a_live_owner_refuses_an_independent_process_without_rewriting_the_lock() {
    let root = directory();
    let path = root.join("namespace.lock");
    fs::write(&path, b"retained lock contents").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let original = Identity::of(&fs::metadata(&path).unwrap());
    let lock = lock_file(&path, owner(&root)).unwrap();
    let output = child(&path, "blocked").wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fs::read(&path).unwrap(), b"retained lock contents");
    drop(lock);
    let output = child(&path, "acquire").wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(Identity::of(&fs::metadata(&path).unwrap()), original);
    assert_eq!(fs::read(&path).unwrap(), b"retained lock contents");
}

#[test]
fn process_death_releases_custody_without_deleting_or_replacing_the_lock() {
    let root = directory();
    let path = root.join("namespace.lock");
    let mut process = child(&path, "hold");
    wait_for_custody(&mut process);
    let original = Identity::of(&fs::metadata(&path).unwrap());
    assert_eq!(lock_file(&path, owner(&root)).unwrap_err(), Error::InUse);
    process.kill().unwrap();
    assert!(!process.wait().unwrap().success());
    let _replacement = lock_file(&path, owner(&root)).unwrap();
    assert_eq!(Identity::of(&fs::metadata(&path).unwrap()), original);
}

#[cfg(target_os = "linux")]
fn inherited_children_outlive_cancelled_owner(path: &Path, uid: u32) {
    use rustix::io::{FdFlags, fcntl_getfd};
    use std::{
        future::{Future, pending},
        os::fd::AsRawFd,
        task::{Context, Poll, Waker},
    };

    let lock = lock_file(path, uid).unwrap();
    // Ordinary lock fixtures stay close-on-exec. Only the production owner
    // (and this isolated subprocess) explicitly opts into child custody.
    assert!(fcntl_getfd(&lock).unwrap().contains(FdFlags::CLOEXEC));
    inherit_for_children(&lock).unwrap();
    let mut first = child_command(path, "inherited")
        .env("FR_HELPER_LOCK_TEST_FD", lock.as_raw_fd().to_string())
        .spawn()
        .unwrap();
    let mut second = child_command(path, "inherited")
        .env("FR_HELPER_LOCK_TEST_FD", lock.as_raw_fd().to_string())
        .spawn()
        .unwrap();
    wait_for_custody(&mut first);
    wait_for_custody(&mut second);

    let instance = Instance {
        _namespace: File::open("/proc/self/ns/net").unwrap(),
        _lock: lock,
    };
    let mut operation = Box::pin(async move {
        let _instance = instance;
        pending::<()>().await;
    });
    assert_eq!(
        operation
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    // Dropping the actual pending owner future represents cancellation
    // while kernel commands are still running. Both exec children retain
    // the same open file description after the owner's guard is gone.
    drop(operation);
    assert_eq!(lock_file(path, uid).unwrap_err(), Error::InUse);

    // A killed command may stop before its sibling. Wait for positive exit,
    // and prove the other command still excludes a replacement helper.
    first.kill().unwrap();
    assert!(!first.wait().unwrap().success());
    assert_eq!(lock_file(path, uid).unwrap_err(), Error::InUse);
    second.stdin.take().unwrap().write_all(b"x").unwrap();
    assert!(second.wait().unwrap().success());
    let _replacement = lock_file(path, uid).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn cancelled_owner_keeps_custody_until_every_exec_child_exits() {
    let root = directory();
    let path = root.join("namespace.lock");
    // Isolate the inheritable descriptor from other tests' concurrently
    // spawned processes; the normal test runner never inherits this lock.
    let output = child(&path, "inherited-parent").wait_with_output().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn lock_paths_refuse_symlinks_hardlinks_wrong_owners_and_shared_permissions() {
    let root = directory();
    let uid = owner(&root);
    let target = root.join("target");
    fs::write(&target, b"untouched").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let link = root.join("symlink");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert_eq!(lock_file(&link, uid).unwrap_err(), Error::Unavailable);
    assert_eq!(lock_file(&root, uid).unwrap_err(), Error::Unavailable);
    assert_eq!(
        lock_file(&target, uid.wrapping_add(1)).unwrap_err(),
        Error::Unavailable
    );
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    assert_eq!(lock_file(&target, uid).unwrap_err(), Error::Unavailable);
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
    let hardlink = root.join("hardlink");
    fs::hard_link(&target, &hardlink).unwrap();
    assert_eq!(lock_file(&target, uid).unwrap_err(), Error::Unavailable);
    assert_eq!(lock_file(&hardlink, uid).unwrap_err(), Error::Unavailable);
    assert_eq!(fs::read(&target).unwrap(), b"untouched");
}

#[test]
fn a_replaced_socket_is_never_removed_by_an_older_stale_snapshot() {
    let root = directory();
    let path = root.join("helper.sock");
    let prior = UnixListener::bind(&path).unwrap();
    let snapshot = Socket::inspect(&path, owner(&root)).unwrap();
    drop(prior);
    fs::rename(&path, root.join("prior.sock")).unwrap();
    let _replacement = UnixListener::bind(&path).unwrap();
    assert_eq!(snapshot.remove_stale(), Err(Error::Unavailable));
    assert!(std::os::unix::net::UnixStream::connect(&path).is_ok());
}

#[test]
fn socket_inspection_refuses_links_regular_files_and_foreign_ownership() {
    let root = directory();
    let uid = owner(&root);
    let absent = Socket::inspect(&root.join("absent.sock"), uid).unwrap();
    assert!(!absent.exists());
    let path = root.join("helper.sock");
    let _listener = UnixListener::bind(&path).unwrap();
    assert!(Socket::inspect(&path, uid).unwrap().exists());
    assert_eq!(
        Socket::inspect(&path, uid.wrapping_add(1)).unwrap_err(),
        Error::Unavailable
    );
    let link = root.join("link.sock");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert_eq!(Socket::inspect(&link, uid).unwrap_err(), Error::Unavailable);
    let file = root.join("file.sock");
    fs::write(&file, b"untouched").unwrap();
    assert_eq!(Socket::inspect(&file, uid).unwrap_err(), Error::Unavailable);
    assert_eq!(fs::read(&file).unwrap(), b"untouched");
}

#[cfg(target_os = "linux")]
#[test]
fn namespace_identity_comes_from_the_retained_kernel_object() {
    let first = File::open("/proc/self/ns/net").unwrap();
    let second = File::open("/proc/self/ns/net").unwrap();
    assert_eq!(
        Identity::of(&first.metadata().unwrap()),
        Identity::of(&second.metadata().unwrap())
    );
}
