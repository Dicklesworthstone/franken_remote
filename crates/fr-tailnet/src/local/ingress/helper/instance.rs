//! Exclusive custody of the helper's nftables namespace.
//!
//! Every helper configuration uses the same `frdh_` table prefix. A socket
//! path is therefore not an ownership boundary: two helpers with different
//! sockets must not reclaim one another's live rules. Keep one kernel lock
//! per network namespace from before the first reclaim until after the last.
//! Neither the lock file nor its contents are removed or rewritten.
use std::{
    fs::{self, File, Metadata, OpenOptions, TryLockError},
    os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Unavailable,
    InUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}
impl Identity {
    fn of(metadata: &Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Holding the namespace descriptor prevents its inode from being recycled
/// while this lock owns tables. Trusted child commands inherit the same open
/// file description, so custody ends only when the helper AND every in-flight
/// command have closed it. Never explicitly unlock: cancellation or process
/// death must not release custody before a surviving command exits.
#[derive(Debug)]
pub(super) struct Instance {
    _namespace: File,
    _lock: File,
}
impl Instance {
    pub(super) fn acquire() -> Result<Self, Error> {
        // The helper is Linux-only at runtime. Its namespace, not a
        // configurable socket path, selects the shared ruleset's owner.
        let namespace = File::open("/proc/self/ns/net").map_err(|_| Error::Unavailable)?;
        let identity = Identity::of(&namespace.metadata().map_err(|_| Error::Unavailable)?);
        let root = Path::new("/run");
        for parent in root.ancestors() {
            let metadata = fs::symlink_metadata(parent).map_err(|_| Error::Unavailable)?;
            if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
                return Err(Error::Unavailable);
            }
        }
        // Keep locks outside the older unit's socket RuntimeDirectory: its
        // stop/failure cleanup must not unlink a live manual helper's lock.
        // The new unit grants this separate RuntimeDirectory write access
        // under ProtectSystem=strict and preserves it across stops. An old
        // unit's read-only /run fails closed until that unit is regenerated.
        let directory = root.join("frankenremote-ingress-locks");
        match fs::symlink_metadata(&directory) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::DirBuilder::new().mode(0o755).create(&directory) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(_) => return Err(Error::Unavailable),
                }
            }
            Err(_) => return Err(Error::Unavailable),
        }
        let metadata = fs::symlink_metadata(&directory).map_err(|_| Error::Unavailable)?;
        if !metadata.is_dir() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(Error::Unavailable);
        }
        let path = directory.join(format!(
            "frankenremote-ingress-{:x}-{:x}.lock",
            identity.device, identity.inode
        ));
        let lock = lock_file(&path, 0)?;
        #[cfg(target_os = "linux")]
        inherit_for_children(&lock)?;
        Ok(Self {
            _namespace: namespace,
            _lock: lock,
        })
    }
}

/// The dedicated helper role only spawns the root-protected nft/ip tools.
/// Asupersync's `Command` delegates to `std::process::Command`, which preserves
/// this descriptor across exec once CLOEXEC is cleared. An interrupted
/// command may outlive its future's kill-on-drop signal; its inherited file
/// description keeps a replacement helper out until kernel execution ends.
/// The trusted tools must retain unknown inherited descriptors while running.
#[cfg(target_os = "linux")]
fn inherit_for_children(lock: &File) -> Result<(), Error> {
    use rustix::io::{FdFlags, fcntl_getfd, fcntl_setfd};
    let flags = fcntl_getfd(lock).map_err(|_| Error::Unavailable)?;
    fcntl_setfd(lock, flags & !FdFlags::CLOEXEC).map_err(|_| Error::Unavailable)
}

#[expect(
    clippy::verbose_bit_mask,
    reason = "Unix permission bits are clearer in octal"
)]
fn valid_lock(metadata: &Metadata, owner: u32) -> bool {
    metadata.is_file()
        && metadata.uid() == owner
        && metadata.mode() & 0o077 == 0
        && metadata.nlink() == 1
}

fn same_lock(path: &Path, file: &Metadata, owner: u32) -> bool {
    fs::symlink_metadata(path).is_ok_and(|current| {
        valid_lock(&current, owner) && Identity::of(&current) == Identity::of(file)
    })
}

/// `owner` is always root in production. Tests use their own uid and isolated
/// directory to exercise the actual kernel lock without privileged paths.
fn lock_file(path: &Path, owner: u32) -> Result<File, Error> {
    let file = match OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let original = fs::symlink_metadata(path).map_err(|_| Error::Unavailable)?;
            if !valid_lock(&original, owner) {
                return Err(Error::Unavailable);
            }
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|_| Error::Unavailable)?;
            let opened = file.metadata().map_err(|_| Error::Unavailable)?;
            if Identity::of(&opened) != Identity::of(&original) {
                return Err(Error::Unavailable);
            }
            file
        }
        Err(_) => return Err(Error::Unavailable),
    };
    let metadata = file.metadata().map_err(|_| Error::Unavailable)?;
    if !valid_lock(&metadata, owner) || !same_lock(path, &metadata, owner) {
        return Err(Error::Unavailable);
    }
    file.try_lock().map_err(|error| match error {
        TryLockError::WouldBlock => Error::InUse,
        TryLockError::Error(_) => Error::Unavailable,
    })?;
    if !same_lock(path, &metadata, owner) {
        return Err(Error::Unavailable);
    }
    Ok(file)
}

/// A candidate stale socket. The liveness check happens asynchronously in the
/// caller; this snapshot prevents removing a different filesystem object after
/// that await. A successful/uncertain connect must never call `remove_stale`.
#[derive(Debug)]
pub(super) struct Socket {
    path: PathBuf,
    original: Option<Identity>,
    owner: u32,
}
impl Socket {
    pub(super) fn inspect(path: &Path, owner: u32) -> Result<Self, Error> {
        let original = match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_socket() && metadata.uid() == owner => {
                Some(Identity::of(&metadata))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            _ => return Err(Error::Unavailable),
        };
        Ok(Self {
            path: path.to_owned(),
            original,
            owner,
        })
    }

    pub(super) const fn exists(&self) -> bool {
        self.original.is_some()
    }

    pub(super) fn remove_stale(self) -> Result<(), Error> {
        let Some(original) = self.original else {
            return Ok(());
        };
        let current = fs::symlink_metadata(&self.path).map_err(|_| Error::Unavailable)?;
        if !current.file_type().is_socket()
            || current.uid() != self.owner
            || Identity::of(&current) != original
        {
            return Err(Error::Unavailable);
        }
        fs::remove_file(self.path).map_err(|_| Error::Unavailable)
    }
}

#[cfg(test)]
#[path = "instance/tests.rs"]
mod tests;
