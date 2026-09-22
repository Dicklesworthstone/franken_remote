//! Handle-relative atomic publication with a stable, nonblocking writer lock.
use super::{Approval, Error, Policy, Sharing};
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

const MAX_BYTES: u64 = 4096;
const LOCK_NAME: &str = ".frd-policy.lock";
const TEMP_PREFIX: &str = ".frd-policy-pending-";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy)]
pub enum Change {
    Approval(Approval),
    Sharing(Sharing),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Saved {
    pub policy: Policy,
    pub changed: bool,
    /// False means rename succeeded but directory fsync failed: publication is
    /// known, crash durability is not. Do not report that nothing was changed.
    pub durable: bool,
}

/// A local file path selected by the operator, never a peer-provided pathname.
/// Each operation pins its directory before I/O; atomic rename means readers
/// see one complete revision. The stable lock serializes field-level updates.
pub struct Store {
    path: PathBuf,
    name: OsString,
}
impl Store {
    pub fn new(path: &Path) -> Result<Self, Error> {
        if !path.is_absolute() || path.as_os_str().len() > 4096 {
            return Err(Error::InvalidPath);
        }
        if path
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
    /// Missing policy uses documented defaults, without creating directories.
    /// Every other error fails closed, including malformed JSON and symlinks.
    pub fn load(&self) -> Result<Policy, Error> {
        let Some(dir) = self.directory(false)? else {
            return Ok(Policy::default());
        };
        self.read(&dir)
    }
    /// Apply one field to the latest revision while holding the local writer
    /// lock. Concurrent approval and sharing saves cannot lose one another.
    pub fn update(&self, change: Change) -> Result<Saved, Error> {
        let dir = self.directory(true)?.ok_or(Error::InvalidPath)?;
        let lock = fs::openat(
            &dir,
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
        let mut policy = self.read(&dir)?;
        let old = policy;
        match change {
            Change::Approval(mode) => policy.approval_mode = mode,
            Change::Sharing(scope) => policy.sharing_scope = scope,
        }
        if policy == old && policy.revision != 0 {
            return Ok(Saved {
                policy,
                changed: false,
                durable: self.sync_existing(&dir).is_ok(),
            });
        }
        policy.revision = policy
            .revision
            .checked_add(1)
            .ok_or(Error::RevisionExhausted)?;
        let mut bytes = serde_json::to_vec(&policy).map_err(|_| Error::InvalidDocument)?;
        bytes.push(b'\n');
        let (name, fd) = temporary(&dir)?;
        // No input policy file is truncated. On failure before rename the old
        // revision remains intact; our own exclusive staging file is removed.
        let result = (|| {
            let mut file = File::from(fd);
            file.write_all(&bytes)?;
            file.sync_all()?;
            fs::renameat(&dir, name.as_str(), &dir, &self.name)?;
            Ok(Saved {
                policy,
                changed: true,
                durable: fs::fsync(&dir).is_ok(),
            })
        })();
        if result.is_err() {
            let _ = fs::unlinkat(&dir, name.as_str(), AtFlags::empty());
        }
        result
    }
    fn sync_existing(&self, dir: &OwnedFd) -> Result<(), Error> {
        let file = fs::openat(
            dir,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        regular_private(&file)?;
        fs::fsync(&file)?;
        fs::fsync(dir)?;
        Ok(())
    }
    fn read(&self, dir: &OwnedFd) -> Result<Policy, Error> {
        let fd = match fs::openat(
            dir,
            &self.name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(Policy::default()),
            Err(e) => return Err(e.into()),
        };
        regular_private(&fd)?;
        if fs::fstat(&fd)?.st_size > i64::try_from(MAX_BYTES).expect("small bound") {
            return Err(Error::TooLarge);
        }
        let mut bytes = Vec::new();
        File::from(fd).take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Error::TooLarge);
        }
        serde_json::from_slice::<Policy>(&bytes)
            .map_err(|_| Error::InvalidDocument)?
            .validate()
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
fn temporary(dir: &OwnedFd) -> Result<(String, OwnedFd), Error> {
    for _ in 0..16 {
        let sequence = NEXT_TEMP
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::RevisionExhausted)?;
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
mod tests;
