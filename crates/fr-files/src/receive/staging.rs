//! Unpublished file custody. Prefer an unnamed inode on the selected filesystem,
//! so even SIGKILL cannot leave a new partial file in the drop directory.
//!
//! Publication links the owned inode through a pinned procfs fd directory. This
//! is Linux's unprivileged alternative to `linkat(AT_EMPTY_PATH)`, not permission
//! to follow a peer-supplied symlink. Both basenames come from the local owner;
//! the destination is still relative to its original, approved directory fd.
use rustix::{
    fd::{AsRawFd, BorrowedFd, OwnedFd},
    fs::{self, AtFlags, Mode, OFlags, RenameFlags},
    io::{Errno, Result},
};
use std::fs::File;

// The procfs handle is acquired before receiving bytes, not after the final
// publication authority check. Its fd table belongs to this process, so an
// inherited transaction must never consult the parent's fd numbers after fork.
struct ProcFd {
    directory: OwnedFd,
    process: u32,
}
impl ProcFd {
    fn open() -> Result<Self> {
        let directory = fs::open(
            "/proc/self/fd",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        if fs::fstatfs(&directory)?.f_type != fs::PROC_SUPER_MAGIC {
            return Err(Errno::NOTSUP);
        }
        Ok(Self {
            directory,
            process: std::process::id(),
        })
    }
}

enum Location {
    Anonymous(ProcFd),
    Named(String),
}

pub(super) struct StagingFile {
    // Close the inode before releasing its directory/procfs handles or quota.
    file: Option<File>,
    location: Location,
}
impl StagingFile {
    pub(super) fn create(directory: BorrowedFd<'_>, name: &str) -> Result<Self> {
        Self::create_with_proc(directory, name, ProcFd::open().ok())
    }

    fn create_with_proc(
        directory: BorrowedFd<'_>,
        name: &str,
        proc: Option<ProcFd>,
    ) -> Result<Self> {
        if let Some(proc) = proc {
            match fs::openat(
                directory,
                ".",
                OFlags::RDWR | OFlags::TMPFILE | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            ) {
                Ok(fd) => {
                    return Ok(Self {
                        file: Some(fd.into()),
                        location: Location::Anonymous(proc),
                    });
                }
                // Only unsupported O_TMPFILE falls back. Disk-full, descriptor
                // exhaustion, permission and other I/O failures remain errors.
                Err(Errno::OPNOTSUPP | Errno::ISDIR | Errno::INVAL) => {}
                Err(error) => return Err(error),
            }
        }
        let fd = fs::openat(
            directory,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        Ok(Self {
            file: Some(fd.into()),
            location: Location::Named(name.to_owned()),
        })
    }

    pub(super) fn file(&mut self) -> &mut File {
        self.file.as_mut().expect("owned until cleanup")
    }

    /// Called only after integrity, file sync and the original authority gate.
    /// No filesystem open or extra staging copy occurs at this final effect.
    pub(super) fn publish(&self, directory: BorrowedFd<'_>, name: &str) -> Result<()> {
        let file = self.file.as_ref().ok_or(Errno::BADF)?;
        match &self.location {
            Location::Anonymous(proc) => {
                if proc.process != std::process::id() {
                    return Err(Errno::BADF);
                }
                fs::linkat(
                    &proc.directory,
                    file.as_raw_fd().to_string(),
                    directory,
                    name,
                    AtFlags::SYMLINK_FOLLOW,
                )
            }
            Location::Named(staging) => fs::renameat_with(
                directory,
                staging.as_str(),
                directory,
                name,
                RenameFlags::NOREPLACE,
            ),
        }
    }

    /// Remove only this transfer's private name, when there is one. Close the
    /// file before the caller can release its byte/transfer reservation.
    pub(super) fn cleanup(&mut self, directory: BorrowedFd<'_>) -> Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        if let Location::Named(name) = &self.location {
            match fs::unlinkat(directory, name.as_str(), AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(error),
            }
        }
        self.file = None;
        Ok(())
    }
}

#[cfg(test)]
#[path = "staging/tests.rs"]
mod tests;
