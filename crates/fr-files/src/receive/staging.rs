//! Unpublished file custody. Prefer an unnamed inode on the selected filesystem,
//! so even SIGKILL cannot leave a new partial file in the drop directory.
//!
//! Publication links the owned inode through a pinned procfs fd directory. This
//! is Linux's unprivileged alternative to `linkat(AT_EMPTY_PATH)`, not permission
//! to follow a peer-supplied symlink. Both basenames come from the local owner;
//! the destination is still relative to its original, approved directory fd.
//! Other platforms have no procfs: they always stage under an exclusively
//! created private name in the destination directory.
#[cfg(target_os = "linux")]
use rustix::fd::{AsRawFd, OwnedFd};
use rustix::{
    fd::BorrowedFd,
    fs::{self, AtFlags, Mode, OFlags, RenameFlags},
    io::{Errno, Result},
};
use std::fs::File;

// The procfs handle is acquired before receiving bytes, not after the final
// publication authority check. Its fd table belongs to this process, so an
// inherited transaction must never consult the parent's fd numbers after fork.
#[cfg(target_os = "linux")]
struct ProcFd {
    directory: OwnedFd,
    process: u32,
}
#[cfg(target_os = "linux")]
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
    #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
        return Self::create_with_proc(directory, name, ProcFd::open().ok());
        #[cfg(not(target_os = "linux"))]
        Self::create_named(directory, name)
    }

    #[cfg(target_os = "linux")]
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
        Self::create_named(directory, name)
    }

    fn create_named(directory: BorrowedFd<'_>, name: &str) -> Result<Self> {
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
        if self.file.is_none() {
            return Err(Errno::BADF);
        }
        match &self.location {
            #[cfg(target_os = "linux")]
            Location::Anonymous(proc) => {
                let file = self.file.as_ref().ok_or(Errno::BADF)?;
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
        match &self.location {
            Location::Named(name) => match fs::unlinkat(directory, name.as_str(), AtFlags::empty())
            {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(error),
            },
            #[cfg(target_os = "linux")]
            Location::Anonymous(_) => {}
        }
        self.file = None;
        Ok(())
    }
}

// The unnamed path, its named fallback and their custody are exercised on
// Linux tmpfs and procfs.
#[cfg(all(test, target_os = "linux"))]
#[path = "staging/tests.rs"]
mod tests;
