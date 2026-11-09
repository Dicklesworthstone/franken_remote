use super::*;
use asupersync::runtime::{Runtime, RuntimeBuilder};
use std::{
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
        .unwrap()
}
fn test_address() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir()
        .join(format!("fr-ctl-{}-{serial}", std::process::id()))
        .join("session.sock")
}
fn uid() -> u32 {
    UnixStream::pair().unwrap().0.peer_cred().unwrap().uid
}
fn failure<T>(result: io::Result<T>) -> io::ErrorKind {
    match result {
        Ok(_) => panic!("unexpected endpoint admission"),
        Err(error) => error.kind(),
    }
}

#[test]
fn competing_pathname_owner_cannot_unlink_the_live_listener() {
    let runtime = runtime();
    runtime.block_on(async {
        let address = test_address();
        let uid = uid();
        let first = bind(&address, uid).await.unwrap();
        let original = std::fs::symlink_metadata(&address).unwrap();
        for _ in 0..4 {
            assert_eq!(failure(bind(&address, uid).await), io::ErrorKind::AddrInUse);
            let unchanged = std::fs::symlink_metadata(&address).unwrap();
            assert_eq!(
                (unchanged.dev(), unchanged.ino()),
                (original.dev(), original.ino())
            );
        }
        let client = dial(&address).await.unwrap();
        let (accepted, _) = first.accept().await.unwrap();
        assert_eq!(client.peer_cred().unwrap().uid, uid);
        assert_eq!(accepted.peer_cred().unwrap().uid, uid);
    });
}

#[test]
fn a_crashed_pathname_owner_is_reclaimed_under_the_same_lock_inode() {
    let runtime = runtime();
    runtime.block_on(async {
        let address = test_address();
        let uid = uid();
        let first = bind(&address, uid).await.unwrap();
        let lock_path = address.with_extension("lock");
        let before = std::fs::symlink_metadata(&lock_path).unwrap();
        drop(first);
        // An orderly drop unlinks the listener's own socket (asupersync's
        // UnixListener cleans up its inode). A crash leaves the socket file
        // behind with no listener: recreate exactly that residue.
        drop(std::os::unix::net::UnixListener::bind(&address).unwrap());
        assert!(
            std::fs::symlink_metadata(&address)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert_eq!(
            failure(dial(&address).await),
            io::ErrorKind::ConnectionRefused
        );
        let second = bind(&address, uid).await.unwrap();
        let after = std::fs::symlink_metadata(lock_path).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        let _client = dial(&address).await.unwrap();
        let _accepted = second.accept().await.unwrap();
        assert_eq!(failure(bind(&address, uid).await), io::ErrorKind::AddrInUse);
    });
}

#[test]
fn live_legacy_listener_without_a_lock_is_preserved() {
    let runtime = runtime();
    runtime.block_on(async {
        let address = test_address();
        let uid = uid();
        private_directory(&address, uid, true).unwrap();
        let legacy = UnixListener::bind(&address).await.unwrap();
        let original = std::fs::symlink_metadata(&address).unwrap();
        assert_eq!(failure(bind(&address, uid).await), io::ErrorKind::AddrInUse);
        let current = std::fs::symlink_metadata(&address).unwrap();
        assert_eq!(
            (current.dev(), current.ino()),
            (original.dev(), original.ino())
        );
        // The attempted stale-owner probe reached the original live listener.
        let _accepted = legacy.accept().await.unwrap();
    });
}

#[test]
fn pathname_publication_requires_the_lock_even_before_a_socket_exists() {
    let runtime = runtime();
    runtime.block_on(async {
        let address = test_address();
        let uid = uid();
        private_directory(&address, uid, true).unwrap();
        let held = lock(&address, uid).unwrap();
        assert_eq!(failure(bind(&address, uid).await), io::ErrorKind::AddrInUse);
        assert_eq!(
            std::fs::symlink_metadata(&address).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        drop(held);
        let _owner = bind(&address, uid).await.unwrap();
    });
}

#[test]
fn public_or_symlinked_endpoint_directories_are_refused() {
    let runtime = runtime();
    runtime.block_on(async {
        let address = test_address();
        let uid = uid();
        let directory = address.parent().unwrap();
        std::fs::create_dir(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            failure(bind(&address, uid).await),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            failure(dial(&address).await),
            io::ErrorKind::PermissionDenied
        );

        let link_address = test_address();
        symlink(directory, link_address.parent().unwrap()).unwrap();
        assert_eq!(
            failure(bind(&link_address, uid).await),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            failure(dial(&link_address).await),
            io::ErrorKind::PermissionDenied
        );
    });
}

#[test]
fn unexpected_socket_files_and_untrusted_lock_files_are_never_replaced() {
    let runtime = runtime();
    runtime.block_on(async {
        let address = test_address();
        let uid = uid();
        private_directory(&address, uid, true).unwrap();
        std::fs::write(&address, b"not a socket").unwrap();
        assert_eq!(
            failure(bind(&address, uid).await),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            failure(dial(&address).await),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(&address).unwrap(), b"not a socket");

        let link_address = address.with_file_name("linked.sock");
        symlink(&address, &link_address).unwrap();
        assert_eq!(
            failure(bind(&link_address, uid).await),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            failure(dial(&link_address).await),
            io::ErrorKind::PermissionDenied
        );

        let public_lock = address.with_file_name("public.sock");
        let path = public_lock.with_extension("lock");
        std::fs::write(&path, b"unchanged").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            failure(bind(&public_lock, uid).await),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(path).unwrap(), b"unchanged");

        let linked_lock = address.with_file_name("lock-link.sock");
        symlink(&address, linked_lock.with_extension("lock")).unwrap();
        assert!(bind(&linked_lock, uid).await.is_err());
        assert_eq!(std::fs::read(&address).unwrap(), b"not a socket");

        let other_uid = uid.wrapping_add(1);
        assert_eq!(
            failure(bind(&address, other_uid).await),
            io::ErrorKind::PermissionDenied
        );
    });
}
