//! Handle-relative local profile storage; readers see one complete revision.
use super::Error;
use fr_client::discovery::saved_peers::{MAX_SAVED_PEER_BYTES, SavedPeer, SavedPeers};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags},
    process::geteuid,
};
use std::{
    ffi::OsString,
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};
const MAX_BYTES: u64 = MAX_SAVED_PEER_BYTES as u64;
const LOCK_NAME: &str = ".fr-saved-hosts.lock";
const TEMP_PREFIX: &str = ".fr-saved-hosts-pending-";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
pub(super) enum Change {
    Add(SavedPeer),
    Forget(String),
}
pub(super) struct Saved {
    pub peers: SavedPeers,
    pub durable: bool,
}
pub(super) struct Store {
    path: PathBuf,
    name: OsString,
}
impl Store {
    pub(super) fn new(path: &Path) -> Result<Self, Error> {
        if !path.is_absolute()
            || path.as_os_str().len() > 4096
            || path
                .components()
                .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
        {
            return Err(Error::InvalidPath);
        }
        let name = path.file_name().ok_or(Error::InvalidPath)?;
        if name.len() > 255 || name == LOCK_NAME || name.to_string_lossy().starts_with(TEMP_PREFIX)
        {
            return Err(Error::InvalidPath);
        }
        Ok(Self {
            path: path.to_owned(),
            name: name.to_owned(),
        })
    }
    /// Missing state is empty, without creating anything. Every malformed or
    /// unsafe existing file refuses; there is no overwrite-on-parse-error path.
    pub(super) fn load(&self) -> Result<SavedPeers, Error> {
        let Some(dir) = self.directory(false)? else {
            return Ok(SavedPeers::default());
        };
        self.read(&dir)
    }
    pub(super) fn update(&self, change: Change) -> Result<Saved, Error> {
        let dir = self.directory(true)?.ok_or(Error::InvalidPath)?;
        let _lock = WriterLock::acquire(&dir)?;
        let mut peers = self.read(&dir)?;
        match change {
            Change::Add(peer) => peers.add(peer)?,
            Change::Forget(alias) => peers.remove(&alias)?,
        }
        let bytes = peers.to_json()?;
        let (name, fd) = temporary(&dir)?;
        let result = (|| {
            let mut file = File::from(fd);
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::renameat(&dir, name.as_str(), &dir, &self.name)?;
            // Publication is known even if its crash durability is uncertain.
            Ok(Saved {
                peers,
                durable: fs::fsync(&dir).is_ok(),
            })
        })();
        if result.is_err() {
            let _ = fs::unlinkat(&dir, name.as_str(), AtFlags::empty());
        }
        result
    }
    fn read(&self, dir: &OwnedFd) -> Result<SavedPeers, Error> {
        let fd = match fs::openat(
            dir,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(SavedPeers::default()),
            Err(e) => return Err(e.into()),
        };
        regular_private(&fd)?;
        if fs::fstat(&fd)?.st_size > i64::try_from(MAX_BYTES).expect("small bound") {
            return Err(Error::TooLarge);
        }
        let mut bytes = Vec::new();
        File::from(fd).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_SAVED_PEER_BYTES {
            return Err(Error::TooLarge);
        }
        SavedPeers::from_json(&bytes).map_err(Into::into)
    }
    fn directory(&self, create: bool) -> Result<Option<OwnedFd>, Error> {
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let mut dir = fs::open("/", flags, Mode::empty())?;
        let parent = self.path.parent().ok_or(Error::InvalidPath)?;
        for component in parent.components() {
            if let Component::Normal(name) = component {
                // An untrusted ancestor could replace an entire private subtree
                // between commands. Root-owned sticky temp roots are safe to
                // traverse; the final selected directory must still be private.
                trusted_ancestor(&dir)?;
                dir = match fs::openat(&dir, name, flags, Mode::empty()) {
                    Ok(fd) => fd,
                    Err(rustix::io::Errno::NOENT) if create => {
                        match fs::mkdirat(&dir, name, Mode::from_raw_mode(0o700)) {
                            Ok(()) => fs::fsync(&dir)?,
                            Err(rustix::io::Errno::EXIST) => {}
                            Err(e) => return Err(e.into()),
                        }
                        fs::openat(&dir, name, flags, Mode::empty())?
                    }
                    Err(rustix::io::Errno::NOENT) => return Ok(None),
                    Err(e) => return Err(e.into()),
                };
            }
        }
        let stat = fs::fstat(&dir)?;
        if stat.st_uid != geteuid().as_raw() || stat.st_mode & 0o022 != 0 {
            return Err(Error::UnsafePath);
        }
        Ok(Some(dir))
    }
}
fn trusted_ancestor(fd: &OwnedFd) -> Result<(), Error> {
    let stat = fs::fstat(fd)?;
    let trusted_owner = stat.st_uid == 0 || stat.st_uid == geteuid().as_raw();
    let sticky_root = trusted_owner && stat.st_mode & 0o1000 != 0;
    if !trusted_owner || (stat.st_mode & 0o022 != 0 && !sticky_root) {
        return Err(Error::UnsafePath);
    }
    Ok(())
}
fn regular_private(fd: &OwnedFd) -> Result<(), Error> {
    let stat = fs::fstat(fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != geteuid().as_raw()
        || stat.st_mode & 0o077 != 0
        || stat.st_nlink != 1
    {
        return Err(Error::UnsafePath);
    }
    Ok(())
}
/// The local writer lock. Dropping it UNLOCKS before closing: another
/// thread's fork (frd spawning a worker) holds a copy of this descriptor until
/// its exec, and a bare close left the lock with that copy, so the next save
/// failed with a spurious `Busy`. An unlock applies to the shared open file
/// description, inherited copies included.
struct WriterLock(OwnedFd);
impl WriterLock {
    fn acquire(dir: &OwnedFd) -> Result<Self, Error> {
        let lock = fs::openat(
            dir,
            LOCK_NAME,
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o600),
        )?;
        regular_private(&lock)?;
        fs::flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                Error::Busy
            } else {
                e.into()
            }
        })?;
        Ok(Self(lock))
    }
}
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = fs::flock(&self.0, FlockOperation::Unlock);
    }
}
fn temporary(dir: &OwnedFd) -> Result<(String, OwnedFd), Error> {
    for _ in 0..16 {
        let sequence = NEXT_TEMP
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Busy)?;
        let name = format!("{TEMP_PREFIX}{}-{sequence}", std::process::id());
        match fs::openat(
            dir,
            name.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        ) {
            Ok(fd) => return Ok((name, fd)),
            Err(rustix::io::Errno::EXIST) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Busy)
}

#[cfg(test)]
#[path = "store/tests.rs"]
mod tests;
