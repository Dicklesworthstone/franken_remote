//! Unpublished file custody. Prefer an unnamed inode on Linux filesystems,
//! so even SIGKILL cannot leave a new partial file in the drop directory.
//!
//! Linux publication links the owned inode through a pinned procfs fd directory.
//! Windows creates a private regular file under the selected directory handle
//! and publishes or removes that exact file handle. Its bounded rename record
//! is prepared before the final authority check. Other Unix platforms retain
//! the exclusive private named-staging path.
#[cfg(target_os = "linux")]
use crate::rx::fd::{AsRawFd, OwnedFd};
#[cfg(not(windows))]
use crate::rx::fs::{self, AtFlags, Mode, OFlags, RenameFlags};
use crate::rx::{
    fd::BorrowedFd,
    io::{Errno, Result},
};
use std::fs::File;
#[cfg(windows)]
use std::os::windows::io::AsHandle;

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

#[cfg(not(windows))]
enum Location {
    #[cfg(target_os = "linux")]
    Anonymous(ProcFd),
    Named(String),
}

pub(super) struct StagingFile {
    // Close the inode before releasing its directory/procfs handles or quota.
    file: Option<File>,
    #[cfg(not(windows))]
    location: Location,
}

/// All native preparation is complete before the caller checks its original
/// authority. Committing this value performs only the final publication call.
pub(super) struct PreparedPublication<'a> {
    #[cfg(windows)]
    native: fr_winsys::fs::PreparedReceiveRename<'a>,
    #[cfg(not(windows))]
    staging: &'a StagingFile,
    #[cfg(not(windows))]
    directory: BorrowedFd<'a>,
    #[cfg(not(windows))]
    name: &'a str,
}
impl PreparedPublication<'_> {
    pub(super) fn commit(self) -> Result<()> {
        #[cfg(windows)]
        return self.native.commit().map_err(Errno::from);
        #[cfg(not(windows))]
        self.staging.publish(self.directory, self.name)
    }
}

impl StagingFile {
    pub(super) fn create(directory: BorrowedFd<'_>, name: &str) -> Result<Self> {
        #[cfg(target_os = "linux")]
        return Self::create_with_proc(directory, name, ProcFd::open().ok());
        #[cfg(windows)]
        {
            let file = fr_winsys::fs::create_receive_file(directory, name).map_err(Errno::from)?;
            Ok(Self { file: Some(file) })
        }
        #[cfg(all(not(target_os = "linux"), not(windows)))]
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

    #[cfg(not(windows))]
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

    pub(super) fn prepare<'a>(
        &'a self,
        directory: BorrowedFd<'a>,
        name: &'a str,
    ) -> Result<PreparedPublication<'a>> {
        #[cfg(windows)]
        {
            let file = self.file.as_ref().ok_or(Errno::BADF)?;
            let native = fr_winsys::fs::prepare_receive_rename(file.as_handle(), directory, name)
                .map_err(Errno::from)?;
            Ok(PreparedPublication { native })
        }
        #[cfg(not(windows))]
        {
            if self.file.is_none() {
                return Err(Errno::BADF);
            }
            Ok(PreparedPublication {
                staging: self,
                directory,
                name,
            })
        }
    }

    /// Called only after integrity, file sync and the original authority gate.
    /// No filesystem open or extra staging copy occurs at this final effect.
    #[cfg(not(windows))]
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

    /// Remove only this transfer's original private object. Close the file
    /// before the caller can release its byte/transfer reservation.
    pub(super) fn cleanup(&mut self, directory: BorrowedFd<'_>) -> Result<()> {
        if self.file.is_none() {
            return Ok(());
        }
        #[cfg(windows)]
        {
            // The original file handle carries deletion authority. Neither a
            // renamed root nor a replacement staging name can redirect cleanup.
            let _ = directory;
            let file = self.file.as_ref().ok_or(Errno::BADF)?;
            match fr_winsys::fs::remove_receive_file(file.as_handle()).map_err(Errno::from) {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(error),
            }
        }
        #[cfg(not(windows))]
        match &self.location {
            Location::Named(name) => match fs::unlinkat(directory, name.as_str(), AtFlags::empty())
            {
                Ok(()) | Err(Errno::NOENT) => {}
                Err(error) => return Err(error),
            },
            #[cfg(target_os = "linux")]
            Location::Anonymous(_) => {}
        }
        // Native delete disposition is completed by closing this sole owned
        // handle, still before PendingFile releases its shared reservation.
        self.file = None;
        Ok(())
    }
}

// The unnamed path, its named fallback and their custody are exercised on
// Linux tmpfs and procfs.
#[cfg(all(test, target_os = "linux"))]
#[path = "staging/tests.rs"]
mod tests;
