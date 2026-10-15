use super::*;
use rustix::fd::AsFd;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

struct Scratch {
    path: PathBuf,
    directory: OwnedFd,
}
impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        // Linux tmpfs qualifies the unnamed path, independent of the checkout's
        // filesystem. Named fallback is explicitly exercised against it too.
        let path = PathBuf::from("/dev/shm").join(format!(
            "fr-staging-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let directory = rustix::fs::open(
            &path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        Self { path, directory }
    }
    fn create(&self, anonymous: bool) -> StagingFile {
        let proc = anonymous.then(|| ProcFd::open().unwrap());
        let file =
            StagingFile::create_with_proc(self.directory.as_fd(), ".fr-part-test", proc).unwrap();
        assert_eq!(matches!(file.location, Location::Anonymous(_)), anonymous);
        file
    }
    fn entries(&self) -> usize {
        fs::read_dir(&self.path).unwrap().count()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).unwrap();
    }
}

#[test]
fn unnamed_publication_links_exact_synced_inode_without_a_payload_copy() {
    let root = Scratch::new();
    let mut staged = root.create(true);
    let original = staged.file().metadata().unwrap();
    assert!(original.is_file());
    assert_eq!(original.nlink(), 0);
    assert_eq!(original.permissions().mode() & 0o777, 0o600);
    let data = vec![91; 131_073];
    for part in data.chunks(65_536) {
        staged.file().write_all(part).unwrap();
        assert_eq!(root.entries(), 0);
    }
    staged.file().sync_all().unwrap();
    staged.publish(root.directory.as_fd(), "complete").unwrap();
    let published = fs::metadata(root.path.join("complete")).unwrap();
    assert_eq!(
        (published.dev(), published.ino()),
        (original.dev(), original.ino())
    );
    assert_eq!(published.nlink(), 1);
    assert_eq!(fs::read(root.path.join("complete")).unwrap(), data);
    // Cleanup is for the original staging object, never its published link.
    staged.cleanup(root.directory.as_fd()).unwrap();
    assert!(staged.file.is_none());
    assert_eq!(root.entries(), 1);
}

#[test]
fn cancellation_and_drop_leave_no_names_or_open_staging_handles() {
    let root = Scratch::new();
    for explicit in [false, true] {
        let mut staged = root.create(true);
        staged.file().write_all(b"unfinished").unwrap();
        if explicit {
            staged.cleanup(root.directory.as_fd()).unwrap();
            assert!(staged.file.is_none());
            staged.cleanup(root.directory.as_fd()).unwrap();
            assert_eq!(
                staged.publish(root.directory.as_fd(), "late"),
                Err(Errno::BADF)
            );
        }
        drop(staged);
        assert_eq!(root.entries(), 0);
    }
}

#[test]
fn neither_backend_overwrites_files_directories_symlinks_or_hardlinks() {
    for anonymous in [false, true] {
        for kind in 0..4 {
            let root = Scratch::new();
            fs::write(root.path.join("original"), b"preserve").unwrap();
            let destination = root.path.join("occupied");
            match kind {
                0 => fs::write(&destination, b"keep").unwrap(),
                1 => fs::create_dir(&destination).unwrap(),
                2 => symlink(root.path.join("original"), &destination).unwrap(),
                _ => fs::hard_link(root.path.join("original"), &destination).unwrap(),
            }
            let before = fs::symlink_metadata(&destination).unwrap();
            let mut staged = root.create(anonymous);
            staged.file().write_all(b"incoming").unwrap();
            for _ in 0..8 {
                assert_eq!(
                    staged.publish(root.directory.as_fd(), "occupied"),
                    Err(Errno::EXIST)
                );
            }
            staged.publish(root.directory.as_fd(), "alternate").unwrap();
            let after = fs::symlink_metadata(&destination).unwrap();
            assert_eq!(
                (after.dev(), after.ino(), after.mode()),
                (before.dev(), before.ino(), before.mode())
            );
            assert_eq!(fs::read(root.path.join("original")).unwrap(), b"preserve");
            assert_eq!(fs::read(root.path.join("alternate")).unwrap(), b"incoming");
            staged.cleanup(root.directory.as_fd()).unwrap();
            assert_eq!(root.entries(), 3);
        }
    }
}

#[test]
fn both_backends_publish_into_the_retained_directory_after_its_path_is_replaced() {
    for anonymous in [false, true] {
        let parent = Scratch::new();
        let selected = parent.path.join("selected");
        fs::create_dir(&selected).unwrap();
        let fd =
            rustix::fs::open(&selected, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty()).unwrap();
        let mut staged = StagingFile::create_with_proc(
            fd.as_fd(),
            ".fr-part-test",
            anonymous.then(|| ProcFd::open().unwrap()),
        )
        .unwrap();
        staged.file().write_all(b"original directory").unwrap();
        fs::rename(&selected, parent.path.join("retained")).unwrap();
        fs::create_dir(&selected).unwrap();
        staged.publish(fd.as_fd(), "complete").unwrap();
        assert_eq!(fs::read_dir(&selected).unwrap().count(), 0);
        assert_eq!(
            fs::read(parent.path.join("retained/complete")).unwrap(),
            b"original directory"
        );
    }
}

#[test]
fn unavailable_procfs_uses_exclusive_private_named_staging_and_cleanup() {
    let root = Scratch::new();
    let mut staged = root.create(false);
    assert_eq!(root.entries(), 1);
    assert_eq!(staged.file().metadata().unwrap().nlink(), 1);
    assert_eq!(
        staged.file().metadata().unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(matches!(
        StagingFile::create_with_proc(root.directory.as_fd(), ".fr-part-test", None),
        Err(Errno::EXIST)
    ));
    staged.cleanup(root.directory.as_fd()).unwrap();
    assert!(staged.file.is_none());
    assert_eq!(root.entries(), 0);
}

#[test]
fn named_cleanup_failure_keeps_custody_for_the_original_owner() {
    let root = Scratch::new();
    let mut staged = root.create(false);
    fs::rename(root.path.join(".fr-part-test"), root.path.join("moved")).unwrap();
    fs::create_dir(root.path.join(".fr-part-test")).unwrap();
    assert_eq!(staged.cleanup(root.directory.as_fd()), Err(Errno::ISDIR));
    assert!(staged.file.is_some());
    fs::remove_dir(root.path.join(".fr-part-test")).unwrap();
    staged.cleanup(root.directory.as_fd()).unwrap();
    assert!(staged.file.is_none());
    assert!(root.path.join("moved").exists());
}

#[test]
fn unnamed_cleanup_does_not_touch_old_staging_or_user_entries() {
    let root = Scratch::new();
    fs::write(root.path.join(".fr-part-test"), b"old owned data").unwrap();
    fs::write(root.path.join("unrelated"), b"user data").unwrap();
    let mut staged = root.create(true);
    staged.file().write_all(b"partial new transfer").unwrap();
    staged.cleanup(root.directory.as_fd()).unwrap();
    assert_eq!(root.entries(), 2);
    assert_eq!(
        fs::read(root.path.join(".fr-part-test")).unwrap(),
        b"old owned data"
    );
    assert_eq!(fs::read(root.path.join("unrelated")).unwrap(), b"user data");
}

#[test]
fn inherited_proc_fd_table_cannot_publish_a_different_process_descriptor() {
    let root = Scratch::new();
    let mut staged = root.create(true);
    let Location::Anonymous(proc) = &mut staged.location else {
        unreachable!()
    };
    proc.process = 0;
    assert_eq!(
        staged.publish(root.directory.as_fd(), "wrong-process"),
        Err(Errno::BADF)
    );
    assert_eq!(root.entries(), 0);
    staged.cleanup(root.directory.as_fd()).unwrap();
}

const CHILD_ROOT: &str = "FR_STAGING_KILL_FIXTURE_ROOT";
#[test]
fn killed_process_fixture() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let fd = rustix::fs::open(
        PathBuf::from(root),
        OFlags::RDONLY | OFlags::DIRECTORY,
        Mode::empty(),
    )
    .unwrap();
    let mut staged = StagingFile::create(fd.as_fd(), ".fr-part-killed").unwrap();
    assert!(matches!(staged.location, Location::Anonymous(_)));
    staged.file().write_all(&[37; 8192]).unwrap();
    staged.file().sync_all().unwrap();
    println!("STAGING_READY");
    std::io::stdout().flush().unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn killing_the_process_reclaims_unpublished_data_without_running_drop() {
    let root = Scratch::new();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["killed_process_fixture", "--nocapture", "--test-threads=1"])
        .env(CHILD_ROOT, &root.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let output = child.stdout.take().unwrap();
    let (send, receive) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let ready = BufReader::new(output)
            .lines()
            .take(10)
            .any(|line| line.is_ok_and(|line| line.contains("STAGING_READY")));
        let _ = send.send(ready);
    });
    let ready = receive.recv_timeout(Duration::from_secs(5));
    let killed = child.kill();
    let status = child.wait().unwrap();
    reader.join().unwrap();
    assert_eq!(ready, Ok(true));
    killed.unwrap();
    assert!(!status.success());
    assert_eq!(
        root.entries(),
        0,
        "SIGKILL must not leave an unpublished inode with a name"
    );
}
