//! Protected macOS pathname sockets, also exercised with real sockets on Linux.
//! The original listener owns its publication lock. Drop never unlinks a path;
//! a subsequent locked owner may reclaim only an unchanged, refused socket.
use super::super::{Scope, TICK};
use rustix::fs::OFlags;
use std::{
    fs::{File, Metadata, OpenOptions, TryLockError},
    io,
    ops::Deref,
    os::{
        fd::{AsFd, BorrowedFd},
        unix::{
            fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    thread,
    time::Instant,
};

pub(in super::super) struct Listener {
    socket: UnixListener,
    // Fields drop in declaration order: close the listener before unlocking.
    _lock: File,
}
impl Deref for Listener {
    type Target = UnixListener;

    fn deref(&self) -> &Self::Target {
        &self.socket
    }
}
impl AsFd for Listener {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.socket.as_fd()
    }
}

pub(super) fn address(scope: Scope, uid: u32, target: &str) -> PathBuf {
    let direction = match scope {
        Scope::Host => 'h',
        Scope::Upload => 'u',
    };
    // Fixed /tmp is the OS namespace, not caller-controlled TMPDIR. The longest
    // accepted 64-byte label and u32 UID fit macOS's 104-byte sun_path.
    PathBuf::from(format!("/tmp/fr-files-{uid}/{direction}-{target}.sock"))
}

pub(super) fn connect(address: &Path, uid: u32, until: Instant) -> io::Result<UnixStream> {
    live(until)?;
    private_directory(address, uid, false)?;
    check_socket(address, uid)?;
    dial(address, until)
}

pub(super) fn bind(address: &Path, uid: u32, until: Instant) -> io::Result<Listener> {
    live(until)?;
    private_directory(address, uid, true)?;
    let lock = lock(address, uid)?;
    match check_socket(address, uid) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    live(until)?;
    let socket = match UnixListener::bind(address) {
        Ok(socket) => socket,
        Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
            let original = check_socket(address, uid)?;
            // Preserve a live older owner even when it predates our lock.
            match dial(address, until) {
                Err(probe) if probe.kind() == io::ErrorKind::ConnectionRefused => {
                    live(until)?;
                    let current = check_socket(address, uid)?;
                    if current.dev() != original.dev() || current.ino() != original.ino() {
                        return Err(error);
                    }
                    std::fs::remove_file(address)?;
                    UnixListener::bind(address)?
                }
                _ => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    socket.set_nonblocking(true)?;
    live(until)?;
    Ok(Listener {
        socket,
        _lock: lock,
    })
}

/// Connect once, nonblocking, under the caller's original handshake deadline.
/// macOS has no `SOCK_NONBLOCK`/`SOCK_CLOEXEC` flags, so set them before connect.
fn dial(address: &Path, until: Instant) -> io::Result<UnixStream> {
    live(until)?;
    let address = rustix::net::SocketAddrUnix::new(address)?;
    let descriptor = rustix::net::socket(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        None,
    )?;
    rustix::io::fcntl_setfd(&descriptor, rustix::io::FdFlags::CLOEXEC)?;
    let stream = UnixStream::from(descriptor);
    stream.set_nonblocking(true)?;
    match rustix::net::connect(&stream, &address) {
        Ok(()) => {}
        Err(rustix::io::Errno::INPROGRESS) => loop {
            live(until)?;
            rustix::net::sockopt::socket_error(&stream)??;
            match stream.peer_addr() {
                Ok(_) => break,
                Err(error) if error.kind() == io::ErrorKind::NotConnected => {
                    thread::sleep(TICK.min(remaining(until)?));
                }
                Err(error) => return Err(error),
            }
        },
        Err(error) => return Err(error.into()),
    }
    live(until)?;
    Ok(stream)
}

fn lock(address: &Path, uid: u32) -> io::Result<File> {
    let path = address.with_extension("lock");
    let flags = i32::try_from((OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC).bits())
        .map_err(|_| io::ErrorKind::InvalidInput)?;
    // Never truncate or follow a pre-existing object. The nonblocking open
    // also prevents an unexpected FIFO from parking the setup caller.
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

fn check_socket(address: &Path, uid: u32) -> io::Result<Metadata> {
    let metadata = std::fs::symlink_metadata(address)?;
    if !metadata.file_type().is_socket() || metadata.uid() != uid || metadata.nlink() != 1 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(metadata)
}

fn private_directory(address: &Path, uid: u32, create: bool) -> io::Result<()> {
    let directory = address.parent().ok_or(io::ErrorKind::InvalidInput)?;
    if create {
        match std::fs::DirBuilder::new().mode(0o700).create(directory) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => return Err(error),
            _ => {}
        }
    }
    // /tmp may itself be the OS-provided /private/tmp link on macOS. The
    // immediate, fixed user directory must be real, private and owned by UID.
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    Ok(())
}

fn remaining(until: Instant) -> io::Result<std::time::Duration> {
    until
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::ErrorKind::TimedOut.into())
}

fn live(until: Instant) -> io::Result<()> {
    remaining(until).map(|_| ())
}

#[cfg(test)]
mod tests;
