//! The Windows store primitives. Each directory from the drive root down is
//! opened (never through a symbolic link or junction) and kept open without
//! sharing deletion, so no one can rename or remove it while this works.
//! Owners and DACLs stand in for Unix owners and mode bits: an ancestor must
//! belong to us or the system and let no one else delete, rename or
//! re-permission its entries (others may add entries, as in a sticky
//! directory); the policy's own directory must be ours and writable by no one
//! else; the policy and lock files must be ours, singly linked and closed to
//! everyone else.
use super::{Error, LOCK_NAME, NEXT_TEMP, Store, TEMP_PREFIX};
use fr_winsys::fs::{self as winfs, Open, Owner};
use std::{
    ffi::OsString,
    fs::{File, OpenOptions, TryLockError},
    io,
    path::{Component, PathBuf},
    sync::atomic::Ordering,
};

pub(super) struct Dir {
    path: PathBuf,
    /// The checked chain, root first, held open until the operation ends.
    _pinned: Vec<File>,
}

fn open_directory(path: &std::path::Path) -> io::Result<File> {
    winfs::open_no_follow(path, Open::Pinned)
}

fn trusted_ancestor(dir: &File) -> Result<(), Error> {
    let found = winfs::inspect(dir)?;
    if !found.directory
        || found.reparse
        || found.owner == Owner::Other
        || found.others_may_replace()
    {
        return Err(Error::UnsafePath);
    }
    Ok(())
}

/// The policy or lock file, checked; its size.
fn regular_private(file: &File) -> Result<u64, Error> {
    let found = winfs::inspect(file)?;
    if found.directory
        || found.reparse
        || found.owner != Owner::Us
        || found.others_may_access()
        || found.links != 1
    {
        return Err(Error::UnsafePath);
    }
    Ok(found.size)
}

impl Store {
    pub(super) fn directory(&self, create: bool) -> Result<Option<Dir>, Error> {
        let parent = self.path.parent().ok_or(Error::InvalidPath)?;
        let mut path = PathBuf::new();
        let mut pinned = Vec::new();
        let mut current: Option<File> = None;
        for component in parent.components() {
            let Component::Normal(name) = component else {
                // The drive and root, which `Store::new` checked.
                path.push(component);
                continue;
            };
            let dir = match current.take() {
                Some(dir) => dir,
                None => open_directory(&path)?,
            };
            trusted_ancestor(&dir)?;
            pinned.push(dir);
            path.push(name);
            current = Some(match open_directory(&path) {
                Ok(dir) => dir,
                Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                    // Created with the parent's inheritable entries; checked
                    // below like any other directory.
                    match std::fs::create_dir(&path) {
                        Ok(()) => {}
                        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(e) => return Err(e.into()),
                    }
                    open_directory(&path)?
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            });
        }
        let dir = match current {
            Some(dir) => dir,
            None => open_directory(&path)?,
        };
        let found = winfs::inspect(&dir)?;
        if !found.directory || found.reparse || found.owner != Owner::Us || found.others_may_write()
        {
            return Err(Error::UnsafePath);
        }
        pinned.push(dir);
        Ok(Some(Dir {
            path,
            _pinned: pinned,
        }))
    }
    pub(super) fn open_policy(&self, dir: &Dir) -> Result<Option<(File, u64)>, Error> {
        // A writer may replace the file while it is open here.
        let file = match winfs::open_no_follow(&dir.path.join(&self.name), Open::Read) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let size = regular_private(&file)?;
        Ok(Some((file, size)))
    }
    pub(super) fn sync_existing(&self, dir: &Dir) -> Result<(), Error> {
        let file = winfs::open_no_follow(&dir.path.join(&self.name), Open::Write)?;
        regular_private(&file)?;
        file.sync_all()?;
        Ok(())
    }
}

/// The local writer lock. Closing the handle releases it too; frd's handles
/// are not inherited by the processes it starts.
pub(super) struct WriterLock(File);
impl WriterLock {
    pub(super) fn acquire(dir: &Dir) -> Result<Self, Error> {
        let lock = winfs::open_no_follow(&dir.path.join(LOCK_NAME), Open::Lock)?;
        regular_private(&lock)?;
        match lock.try_lock() {
            Ok(()) => Ok(Self(lock)),
            Err(TryLockError::WouldBlock) => Err(Error::Busy),
            Err(TryLockError::Error(e)) => Err(e.into()),
        }
    }
}
impl Drop for WriterLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Publishes the staged revision; a reader holding the old one open does
/// not block it. True when the rename was also flushed to disk.
pub(super) fn replace(dir: &Dir, staged: &str, name: &OsString) -> Result<bool, Error> {
    Ok(winfs::replace(
        &dir.path.join(staged),
        &dir.path.join(name),
    )?)
}
pub(super) fn discard(dir: &Dir, staged: &str) {
    let _ = std::fs::remove_file(dir.path.join(staged));
}
pub(super) fn temporary(dir: &Dir) -> Result<(String, File), Error> {
    for _ in 0..16 {
        let sequence = NEXT_TEMP
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::RevisionExhausted)?;
        let name = format!("{TEMP_PREFIX}{}-{sequence}", std::process::id());
        // Create-new never opens an existing file or follows a link.
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.path.join(&name))
        {
            Ok(file) => return Ok((name, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    Err(Error::Busy)
}

// Tested on Windows through the public API: tests/host_policy_windows.rs.
