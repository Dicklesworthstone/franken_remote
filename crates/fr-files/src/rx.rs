//! The handle-relative file-system calls this crate makes.
//!
//! Unix: rustix itself, and std's Unix metadata. Windows regular-file receive
//! uses the safe retained-handle custody in fr-winsys directly. The general
//! directory operations below remain unsupported there: tree receive and
//! directory selection are refused, and directory durability is never invented.
#[cfg(unix)]
pub(crate) use rustix::{fd, fs, io, process};
#[cfg(unix)]
pub(crate) use std::os::unix::fs::MetadataExt;

#[cfg(windows)]
pub(crate) use windows::{MetadataExt, fd, fs, io};

// Items mirror rustix; some are named only by Linux-only paths.
#[cfg(windows)]
#[allow(
    dead_code,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    clippy::unused_self,
    clippy::struct_field_names
)]
mod windows {
    pub(crate) mod fd {
        pub(crate) use std::os::windows::io::{
            BorrowedHandle as BorrowedFd, OwnedHandle as OwnedFd,
        };
        /// rustix's `AsFd`, for any Windows handle owner.
        pub(crate) trait AsFd {
            fn as_fd(&self) -> BorrowedFd<'_>;
        }
        impl<T: std::os::windows::io::AsHandle + ?Sized> AsFd for T {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.as_handle()
            }
        }
    }

    pub(crate) mod io {
        use std::io::ErrorKind;

        /// Sanitized filesystem failures shared with the Unix adapter.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) struct Errno(ErrorKind);
        impl Errno {
            pub(crate) const NOENT: Self = Self(ErrorKind::NotFound);
            pub(crate) const BADF: Self = Self(ErrorKind::Other);
            pub(crate) const EXIST: Self = Self(ErrorKind::AlreadyExists);
            pub(crate) const ISDIR: Self = Self(ErrorKind::IsADirectory);
            pub(crate) const INVAL: Self = Self(ErrorKind::InvalidInput);
            pub(crate) const NOTSUP: Self = Self(ErrorKind::Unsupported);
            pub(crate) const OPNOTSUPP: Self = Self::NOTSUP;
        }
        impl From<std::io::Error> for Errno {
            fn from(value: std::io::Error) -> Self {
                Self(value.kind())
            }
        }
        impl From<Errno> for std::io::Error {
            fn from(value: Errno) -> Self {
                value.0.into()
            }
        }
        pub(crate) type Result<T> = core::result::Result<T, Errno>;
    }

    pub(crate) mod process {
        /// Never compared: every Windows directory open refuses first.
        pub(crate) struct Uid;
        impl Uid {
            pub(crate) const fn as_raw(&self) -> u32 {
                u32::MAX
            }
        }
        pub(crate) const fn geteuid() -> Uid {
            Uid
        }
    }

    pub(crate) mod fs {
        use super::{
            fd::{AsFd, OwnedFd},
            io::{Errno, Result},
        };
        use std::ops::{BitOr, BitOrAssign};

        macro_rules! flags {
            ($name:ident { $($flag:ident = $bit:expr),* $(,)? }) => {
                #[derive(Debug, Clone, Copy, PartialEq, Eq)]
                pub(crate) struct $name(u32);
                impl $name {
                    $(pub(crate) const $flag: Self = Self($bit);)*
                    pub(crate) const fn empty() -> Self {
                        Self(0)
                    }
                }
                impl BitOr for $name {
                    type Output = Self;
                    fn bitor(self, other: Self) -> Self {
                        Self(self.0 | other.0)
                    }
                }
                impl BitOrAssign for $name {
                    fn bitor_assign(&mut self, other: Self) {
                        self.0 |= other.0;
                    }
                }
            };
        }
        flags!(OFlags {
            RDONLY = 0,
            WRONLY = 1 << 0,
            RDWR = 1 << 1,
            CREATE = 1 << 2,
            EXCL = 1 << 3,
            DIRECTORY = 1 << 4,
            NOFOLLOW = 1 << 5,
            CLOEXEC = 1 << 6,
            NONBLOCK = 1 << 7,
            TMPFILE = 1 << 8,
        });
        flags!(AtFlags {
            SYMLINK_FOLLOW = 1 << 0,
            SYMLINK_NOFOLLOW = 1 << 1,
            REMOVEDIR = 1 << 2,
        });
        flags!(RenameFlags { NOREPLACE = 1 << 0 });
        flags!(Mode {});
        impl Mode {
            pub(crate) const fn from_raw_mode(mode: u32) -> Self {
                Self(mode)
            }
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(crate) enum FileType {
            RegularFile,
            Directory,
            Unknown,
        }
        impl FileType {
            pub(crate) const fn from_raw_mode(_mode: u32) -> Self {
                Self::Unknown
            }
        }
        /// Never produced: every call that would return one refuses.
        pub(crate) struct Stat {
            pub(crate) st_dev: u64,
            pub(crate) st_ino: u64,
            pub(crate) st_mode: u32,
            pub(crate) st_uid: u32,
        }

        pub(crate) fn open<P>(_path: P, _flags: OFlags, _mode: Mode) -> Result<OwnedFd> {
            Err(Errno::NOTSUP)
        }
        pub(crate) fn openat<Fd: AsFd, P>(
            _dir: Fd,
            _path: P,
            _flags: OFlags,
            _mode: Mode,
        ) -> Result<OwnedFd> {
            Err(Errno::NOTSUP)
        }
        pub(crate) fn mkdirat<Fd: AsFd, P>(_dir: Fd, _path: P, _mode: Mode) -> Result<()> {
            Err(Errno::NOTSUP)
        }
        pub(crate) fn unlinkat<Fd: AsFd, P>(_dir: Fd, _path: P, _flags: AtFlags) -> Result<()> {
            Err(Errno::NOTSUP)
        }
        pub(crate) fn renameat_with<A: AsFd, P, B: AsFd, Q>(
            _old_dir: A,
            _old: P,
            _new_dir: B,
            _new: Q,
            _flags: RenameFlags,
        ) -> Result<()> {
            Err(Errno::NOTSUP)
        }
        pub(crate) fn fsync<Fd: AsFd>(_fd: Fd) -> Result<()> {
            Err(Errno::NOTSUP)
        }
        pub(crate) fn fstat<Fd: AsFd>(_fd: Fd) -> Result<Stat> {
            Err(Errno::NOTSUP)
        }
        pub(crate) fn statat<Fd: AsFd, P>(_dir: Fd, _path: P, _flags: AtFlags) -> Result<Stat> {
            Err(Errno::NOTSUP)
        }

        /// A directory listing; never opens.
        pub(crate) struct Dir;
        pub(crate) struct DirEntry;
        impl Dir {
            pub(crate) fn read_from<Fd: AsFd>(_fd: Fd) -> Result<Self> {
                Err(Errno::NOTSUP)
            }
            pub(crate) fn read(&mut self) -> Option<Result<DirEntry>> {
                None
            }
        }
        impl DirEntry {
            pub(crate) fn file_name(&self) -> &std::ffi::CStr {
                c""
            }
        }
    }

    /// Unix metadata fields from Windows metadata. Stable Rust exposes no
    /// volume serial or file index, so `dev` and `ino` are 0 and a source's
    /// identity is its length plus its write and creation times (100 ns
    /// ticks since 1601, split into seconds and nanoseconds).
    pub(crate) trait MetadataExt {
        fn dev(&self) -> u64;
        fn ino(&self) -> u64;
        fn mtime(&self) -> i64;
        fn mtime_nsec(&self) -> i64;
        fn ctime(&self) -> i64;
        fn ctime_nsec(&self) -> i64;
    }
    const TICKS: u64 = 10_000_000;
    #[allow(clippy::cast_possible_wrap, reason = "u64 ticks / 10^7 fit i64")]
    const fn seconds(ticks: u64) -> i64 {
        (ticks / TICKS) as i64
    }
    #[allow(clippy::cast_possible_wrap, reason = "below 10^9")]
    const fn nanoseconds(ticks: u64) -> i64 {
        ((ticks % TICKS) * 100) as i64
    }
    impl MetadataExt for std::fs::Metadata {
        fn dev(&self) -> u64 {
            0
        }
        fn ino(&self) -> u64 {
            0
        }
        fn mtime(&self) -> i64 {
            seconds(std::os::windows::fs::MetadataExt::last_write_time(self))
        }
        fn mtime_nsec(&self) -> i64 {
            nanoseconds(std::os::windows::fs::MetadataExt::last_write_time(self))
        }
        fn ctime(&self) -> i64 {
            seconds(std::os::windows::fs::MetadataExt::creation_time(self))
        }
        fn ctime_nsec(&self) -> i64 {
            nanoseconds(std::os::windows::fs::MetadataExt::creation_time(self))
        }
    }
}
