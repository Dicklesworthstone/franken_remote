//! The Unix primitives frd's supervised native children use: socket pairs,
//! process groups, descriptor polling, kernel clocks and file ownership.
//!
//! Unix: exactly std's Unix modules and rustix. Windows (`windows-viewer`)
//! has no supervised child yet: every socket pair refuses `Unsupported`, so
//! no input, microphone, clipboard or approval child is ever spawned there,
//! and the socket types are uninhabited. A Windows file selection opens
//! read-only without following reparse points; directory selections and
//! drop directories refuse. Delete the Windows half once Windows children
//! have their own channels.
#[cfg(unix)]
pub(crate) use ::rustix;
#[cfg(unix)]
pub(crate) use std::os::{fd, unix};

#[cfg(windows)]
pub(crate) use windows::{fd, rustix, unix};

#[cfg(windows)]
#[allow(
    dead_code,
    clippy::unnecessary_wraps,
    clippy::needless_pass_by_value,
    clippy::unused_self,
    clippy::upper_case_acronyms,
    reason = "mirrors rustix's names and signatures"
)]
mod windows {
    pub(crate) mod fd {
        pub(crate) use std::os::windows::io::OwnedHandle as OwnedFd;
    }

    pub(crate) mod unix {
        pub(crate) mod net {
            use std::time::Duration;
            use std::{convert::Infallible, io, net::Shutdown, os::windows::io::OwnedHandle};

            fn unsupported<T>() -> io::Result<T> {
                Err(io::ErrorKind::Unsupported.into())
            }
            /// Uninhabited: [`UnixStream::pair`] always refuses.
            #[derive(Debug)]
            pub(crate) struct UnixStream(Infallible);
            /// Uninhabited: [`UnixDatagram::pair`] always refuses.
            #[derive(Debug)]
            pub(crate) struct UnixDatagram(Infallible);
            macro_rules! socket {
                ($name:ident) => {
                    impl $name {
                        pub(crate) fn pair() -> io::Result<(Self, Self)> {
                            unsupported()
                        }
                        pub(crate) fn set_nonblocking(&self, _: bool) -> io::Result<()> {
                            match self.0 {}
                        }
                        pub(crate) fn set_read_timeout(
                            &self,
                            _: Option<Duration>,
                        ) -> io::Result<()> {
                            match self.0 {}
                        }
                        pub(crate) fn set_write_timeout(
                            &self,
                            _: Option<Duration>,
                        ) -> io::Result<()> {
                            match self.0 {}
                        }
                        pub(crate) fn shutdown(&self, _: Shutdown) -> io::Result<()> {
                            match self.0 {}
                        }
                        pub(crate) fn try_clone(&self) -> io::Result<Self> {
                            match self.0 {}
                        }
                    }
                    impl From<$name> for OwnedHandle {
                        fn from(socket: $name) -> Self {
                            match socket.0 {}
                        }
                    }
                };
            }
            socket!(UnixStream);
            socket!(UnixDatagram);
            impl UnixDatagram {
                pub(crate) fn send(&self, _: &[u8]) -> io::Result<usize> {
                    match self.0 {}
                }
                pub(crate) fn recv(&self, _: &mut [u8]) -> io::Result<usize> {
                    match self.0 {}
                }
            }
            impl io::Read for UnixStream {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    match self.0 {}
                }
            }
            impl io::Read for &UnixStream {
                fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                    match self.0 {}
                }
            }
            impl io::Write for UnixStream {
                fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                    match self.0 {}
                }
                fn flush(&mut self) -> io::Result<()> {
                    match self.0 {}
                }
            }
            impl io::Write for &UnixStream {
                fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                    match self.0 {}
                }
                fn flush(&mut self) -> io::Result<()> {
                    match self.0 {}
                }
            }
        }

        pub(crate) mod process {
            /// `process_group(0)` as Windows has it: the child leads a new
            /// process group (`CREATE_NEW_PROCESS_GROUP`).
            pub(crate) trait CommandExt {
                fn process_group(&mut self, group: i32) -> &mut Self;
            }
            impl CommandExt for std::process::Command {
                fn process_group(&mut self, _group: i32) -> &mut Self {
                    std::os::windows::process::CommandExt::creation_flags(self, 0x0000_0200)
                }
            }
        }

        pub(crate) mod fs {
            /// No Unix owner or mode on Windows: an owner never matches
            /// (`geteuid`), so a drop directory is refused as not owned.
            pub(crate) trait MetadataExt {
                fn uid(&self) -> u32;
                fn dev(&self) -> u64;
                fn ino(&self) -> u64;
            }
            impl MetadataExt for std::fs::Metadata {
                fn uid(&self) -> u32 {
                    0
                }
                fn dev(&self) -> u64 {
                    0
                }
                fn ino(&self) -> u64 {
                    0
                }
            }
            pub(crate) trait PermissionsExt {
                fn mode(&self) -> u32;
            }
            impl PermissionsExt for std::fs::Permissions {
                fn mode(&self) -> u32 {
                    0o777
                }
            }
        }
    }

    pub(crate) mod rustix {
        pub(crate) mod io {
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub(crate) struct Errno(i32);
            impl Errno {
                pub(crate) const INTR: Self = Self(4);
                pub(crate) const NOTSUP: Self = Self(95);
            }
            impl From<Errno> for std::io::Error {
                fn from(value: Errno) -> Self {
                    if value == Errno::INTR {
                        std::io::ErrorKind::Interrupted.into()
                    } else {
                        std::io::ErrorKind::Unsupported.into()
                    }
                }
            }
        }

        pub(crate) mod time {
            use std::{sync::OnceLock, time::Instant};
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub(crate) enum ClockId {
                Monotonic,
            }
            pub(crate) struct Timespec {
                pub(crate) tv_sec: i64,
                pub(crate) tv_nsec: i64,
            }
            /// Monotonic time since this process first asked, in one domain
            /// shared by every caller in the process (Windows has no child
            /// that would need the kernel's).
            pub(crate) fn clock_gettime(_clock: ClockId) -> Timespec {
                static ORIGIN: OnceLock<Instant> = OnceLock::new();
                let elapsed = ORIGIN.get_or_init(Instant::now).elapsed();
                Timespec {
                    tv_sec: i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX),
                    tv_nsec: i64::from(elapsed.subsec_nanos()),
                }
            }
        }

        pub(crate) mod process {
            /// No process group is ever created by a refused spawn; never
            /// constructed on Windows.
            pub(crate) struct Pid(std::convert::Infallible);
            impl Pid {
                pub(crate) const fn from_raw(_: i32) -> Option<Self> {
                    None
                }
            }
            pub(crate) enum Signal {
                KILL,
            }
            pub(crate) fn kill_process_group(pid: Pid, _: Signal) -> Result<(), super::io::Errno> {
                match pid.0 {}
            }
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

        pub(crate) mod event {
            use super::io::Errno;
            use std::{marker::PhantomData, time::Duration};
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub(crate) struct PollFlags(u8);
            impl PollFlags {
                pub(crate) const IN: Self = Self(1);
                pub(crate) const OUT: Self = Self(2);
            }
            /// Only the uninhabited sockets above are ever polled.
            pub(crate) struct PollFd<'a>(PhantomData<&'a ()>);
            impl<'a> PollFd<'a> {
                pub(crate) fn new<T>(_: &'a T, _: PollFlags) -> Self {
                    Self(PhantomData)
                }
            }
            pub(crate) struct Timespec;
            impl TryFrom<Duration> for Timespec {
                type Error = Errno;
                fn try_from(_: Duration) -> Result<Self, Errno> {
                    Ok(Self)
                }
            }
            pub(crate) fn poll(_: &mut [PollFd<'_>], _: Option<&Timespec>) -> Result<usize, Errno> {
                Err(Errno::NOTSUP)
            }
        }

        pub(crate) mod fs {
            use super::io::Errno;
            use std::{
                ops::{BitOr, BitOrAssign},
                os::windows::{fs::OpenOptionsExt, io::OwnedHandle},
                path::Path,
            };
            #[derive(Debug, Clone, Copy, PartialEq, Eq)]
            pub(crate) struct OFlags(u32);
            impl OFlags {
                pub(crate) const RDONLY: Self = Self(0);
                pub(crate) const NOFOLLOW: Self = Self(1);
                pub(crate) const NONBLOCK: Self = Self(2);
                pub(crate) const CLOEXEC: Self = Self(4);
                pub(crate) const DIRECTORY: Self = Self(8);
            }
            impl BitOr for OFlags {
                type Output = Self;
                fn bitor(self, other: Self) -> Self {
                    Self(self.0 | other.0)
                }
            }
            impl BitOrAssign for OFlags {
                fn bitor_assign(&mut self, other: Self) {
                    self.0 |= other.0;
                }
            }
            pub(crate) struct Mode;
            impl Mode {
                pub(crate) const fn empty() -> Self {
                    Self
                }
            }
            /// A read-only open that never follows a reparse point
            /// (`FILE_FLAG_OPEN_REPARSE_POINT`). Directories refuse: a
            /// directory source needs handle-relative listing.
            pub(crate) fn open(path: &Path, flags: OFlags, _: Mode) -> Result<OwnedHandle, Errno> {
                if flags.0 & OFlags::DIRECTORY.0 != 0 {
                    return Err(Errno::NOTSUP);
                }
                std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(0x0020_0000)
                    .open(path)
                    .map(OwnedHandle::from)
                    .map_err(|_| Errno::NOTSUP)
            }
        }
    }
}
