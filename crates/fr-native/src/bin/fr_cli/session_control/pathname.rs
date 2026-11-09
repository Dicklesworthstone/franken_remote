//! macOS's protected pathname endpoint. Compiled on Linux in tests as well,
//! so stale-socket and simultaneous-owner behavior use real kernel sockets.
use asupersync::net::unix::{UnixListener, UnixStream};
use rustix::fs::OFlags;
use std::{
    fs::{File, OpenOptions, TryLockError},
    io,
    ops::Deref,
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    path::Path,
};

/// The listener closes before its lock is released. Socket and lock names
/// remain in the private directory; only a subsequent locked owner may reclaim
/// a socket proven to have no listener. Dropping never unlinks a successor.
pub(super) struct Listener {
    listener: UnixListener,
    _lock: File,
}
impl Deref for Listener {
    type Target = UnixListener;

    fn deref(&self) -> &Self::Target {
        &self.listener
    }
}

/// No caller may reach a socket through a public directory or symlink. The
/// caller still authenticates the connected peer with kernel credentials.
pub(super) async fn dial(address: &Path) -> io::Result<UnixStream> {
    let uid = UnixStream::pair()?.0.peer_cred()?.uid;
    private_directory(address, uid, false)?;
    check_socket(address, uid)?;
    UnixStream::connect(address).await
}

/// Serialize every pathname publication and stale reclaim for this target.
/// An ordinary concurrent startup cannot unlink a just-published live owner.
pub(super) async fn bind(address: &Path, uid: u32) -> io::Result<Listener> {
    private_directory(address, uid, true)?;
    let lock = lock(address, uid)?;
    match check_socket(address, uid) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let listener = match UnixListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            // The lock stays held across the probe and replacement. A live
            // older binary that does not hold the lock is still preserved.
            match UnixStream::connect(address).await {
                Err(probe) if probe.kind() == io::ErrorKind::ConnectionRefused => {
                    check_socket(address, uid)?;
                    std::fs::remove_file(address)?;
                    UnixListener::bind(address).await?
                }
                _ => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    Ok(Listener {
        listener,
        _lock: lock,
    })
}

fn lock(address: &Path, uid: u32) -> io::Result<File> {
    let path = address.with_extension("lock");
    // No truncation: an unexpected existing file is inspected, never changed.
    let flags = i32::try_from((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC).bits())
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(flags)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o077 != 0
        || metadata.nlink() != 1
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => io::Error::from(io::ErrorKind::AddrInUse),
        TryLockError::Error(error) => error,
    })?;
    // Cooperating owners never replace the lock file. Refuse an already
    // replaced pathname instead of publishing under an unrelated inode lock.
    let current = std::fs::symlink_metadata(path)?;
    if !current.is_file()
        || current.dev() != metadata.dev()
        || current.ino() != metadata.ino()
        || current.nlink() != 1
    {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

fn check_socket(address: &Path, uid: u32) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(address)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

/// The immediate parent must be a real private directory owned by this user.
/// `/tmp` itself may be the OS-provided `/private/tmp` link on macOS.
fn private_directory(address: &Path, uid: u32, create: bool) -> io::Result<()> {
    let directory = address.parent().ok_or(io::ErrorKind::InvalidInput)?;
    if create {
        match std::fs::DirBuilder::new().mode(0o700).create(directory) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
    }
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "pathname_tests.rs"]
mod tests;
