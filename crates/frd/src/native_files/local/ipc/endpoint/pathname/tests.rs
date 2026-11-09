use super::*;
use std::{
    io::{Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

fn target() -> PathBuf {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let next = SERIAL.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("fr-file-ipc-{}-{next}", std::process::id()))
        .join("owner.sock")
}

fn uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn until() -> Instant {
    Instant::now() + Duration::from_secs(2)
}

fn failure<T>(result: io::Result<T>) -> io::ErrorKind {
    match result {
        Ok(_) => panic!("unexpected pathname admission"),
        Err(error) => error.kind(),
    }
}

#[test]
fn actual_pathname_connections_retain_kernel_identity_and_transfer_bytes() {
    let path = target();
    let listener = bind(&path, uid(), until()).unwrap();
    let mut client = connect(&path, uid(), until()).unwrap();
    let (mut accepted, _) = listener.accept().unwrap();
    assert_eq!(super::super::peer_uid(&client).unwrap(), uid());
    assert_eq!(super::super::peer_uid(&accepted).unwrap(), uid());
    // Explicit blocking test reads only; production peers stay nonblocking.
    client.set_nonblocking(false).unwrap();
    accepted.set_nonblocking(false).unwrap();
    accepted
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    client.write_all(b"original-owner").unwrap();
    let mut bytes = [0; 14];
    accepted.read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"original-owner");
}

#[test]
fn concurrent_owner_preserves_the_original_listener_and_lock_inode() {
    let path = target();
    let listener = bind(&path, uid(), until()).unwrap();
    let original = std::fs::symlink_metadata(&path).unwrap();
    let lock = std::fs::symlink_metadata(path.with_extension("lock")).unwrap();
    for _ in 0..4 {
        assert_eq!(
            failure(bind(&path, uid(), until())),
            io::ErrorKind::AddrInUse
        );
        let current = std::fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (current.dev(), current.ino()),
            (original.dev(), original.ino())
        );
    }
    let _client = connect(&path, uid(), until()).unwrap();
    let _accepted = listener.accept().unwrap();
    let current = std::fs::symlink_metadata(path.with_extension("lock")).unwrap();
    assert_eq!((current.dev(), current.ino()), (lock.dev(), lock.ino()));
}

#[test]
fn stale_socket_is_reclaimed_only_after_the_original_listener_releases_its_lock() {
    let path = target();
    let original = bind(&path, uid(), until()).unwrap();
    let lock = std::fs::symlink_metadata(path.with_extension("lock")).unwrap();
    drop(original);
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_socket()
    );
    assert_eq!(
        failure(connect(&path, uid(), until())),
        io::ErrorKind::ConnectionRefused
    );
    let successor = bind(&path, uid(), until()).unwrap();
    let _client = connect(&path, uid(), until()).unwrap();
    let _accepted = successor.accept().unwrap();
    let current = std::fs::symlink_metadata(path.with_extension("lock")).unwrap();
    assert_eq!((current.dev(), current.ino()), (lock.dev(), lock.ino()));
}

#[test]
fn live_legacy_owner_without_a_publication_lock_is_preserved() {
    let path = target();
    private_directory(&path, uid(), true).unwrap();
    let legacy = UnixListener::bind(&path).unwrap();
    let original = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        failure(bind(&path, uid(), until())),
        io::ErrorKind::AddrInUse
    );
    let current = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        (current.dev(), current.ino()),
        (original.dev(), original.ino())
    );
    let _probe = legacy.accept().unwrap();
    let _client = connect(&path, uid(), until()).unwrap();
    let _accepted = legacy.accept().unwrap();
}

#[test]
fn foreign_identity_public_directory_and_symlink_directory_are_refused() {
    let path = target();
    private_directory(&path, uid(), true).unwrap();
    assert_eq!(
        failure(connect(&path, uid().wrapping_add(1), until())),
        io::ErrorKind::PermissionDenied
    );
    let public = target();
    std::fs::create_dir(public.parent().unwrap()).unwrap();
    std::fs::set_permissions(
        public.parent().unwrap(),
        std::fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    assert_eq!(
        failure(bind(&public, uid(), until())),
        io::ErrorKind::PermissionDenied
    );
    let linked = target();
    symlink(path.parent().unwrap(), linked.parent().unwrap()).unwrap();
    assert_eq!(
        failure(connect(&linked, uid(), until())),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        failure(bind(&linked, uid(), until())),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn a_non_socket_or_symlink_at_the_endpoint_is_never_replaced() {
    let path = target();
    private_directory(&path, uid(), true).unwrap();
    std::fs::write(&path, b"keep this file").unwrap();
    assert_eq!(
        failure(bind(&path, uid(), until())),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        failure(connect(&path, uid(), until())),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"keep this file");

    let linked = target();
    private_directory(&linked, uid(), true).unwrap();
    symlink(&path, &linked).unwrap();
    assert_eq!(
        failure(bind(&linked, uid(), until())),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"keep this file");
}

#[test]
fn publication_refuses_a_shared_or_symlink_lock_without_modifying_it() {
    let path = target();
    private_directory(&path, uid(), true).unwrap();
    let lock = path.with_extension("lock");
    std::fs::write(&lock, b"original lock").unwrap();
    std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert_eq!(
        failure(bind(&path, uid(), until())),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(std::fs::read(&lock).unwrap(), b"original lock");

    let linked = target();
    private_directory(&linked, uid(), true).unwrap();
    symlink(&lock, linked.with_extension("lock")).unwrap();
    assert!(bind(&linked, uid(), until()).is_err());
    assert_eq!(std::fs::read(&lock).unwrap(), b"original lock");
    assert!(!linked.exists());
}

#[test]
fn expired_setup_and_connect_do_not_create_or_replace_an_endpoint() {
    let path = target();
    assert_eq!(
        failure(bind(&path, uid(), Instant::now())),
        io::ErrorKind::TimedOut
    );
    assert!(!path.parent().unwrap().exists());
    private_directory(&path, uid(), true).unwrap();
    std::fs::write(&path, b"untouched endpoint").unwrap();
    let original = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        failure(connect(&path, uid(), Instant::now())),
        io::ErrorKind::TimedOut
    );
    assert_eq!(
        failure(bind(&path, uid(), Instant::now())),
        io::ErrorKind::TimedOut
    );
    let current = std::fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        (current.dev(), current.ino()),
        (original.dev(), original.ino())
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"untouched endpoint");
}

#[test]
fn bounded_names_fit_macos_and_host_upload_namespaces_do_not_overlap() {
    let label = "x".repeat(64);
    let host = address(Scope::Host, u32::MAX, &label);
    let upload = address(Scope::Upload, u32::MAX, &label);
    assert_ne!(host, upload);
    assert!(host.as_os_str().len() < 104);
    assert!(upload.as_os_str().len() < 104);
    assert_eq!(host.parent(), upload.parent());
}
