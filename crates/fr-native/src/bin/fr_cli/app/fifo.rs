//! Launch Services writes into a private FIFO, never an accumulating file.
//! Its waiter and FIFO are polled together so failure before opening stdout
//! cannot leave the app blocked on an unopened pipe. The viewer's own report
//! supplies its outcome; `open` only supplies launch/wait evidence.
#[cfg(target_os = "macos")]
use super::Finished;
use super::{DRAIN_TIMEOUT, Error, follow};
#[cfg(target_os = "macos")]
use std::path::Path;
use std::{
    fs::{DirBuilder, File},
    io,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const START_TIMEOUT: Duration = Duration::from_secs(15);

struct PrivatePipe {
    directory: PathBuf,
    path: PathBuf,
}

impl PrivatePipe {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..16 {
            let directory = std::env::temp_dir().join(format!(
                "fr-app-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {
                    let owner = Self {
                        path: directory.join("stdout"),
                        directory,
                    };
                    // rustix does not expose FIFO creation on Apple targets.
                    // Use the OS utility directly, without a shell or captured
                    // output; the private parent already owns this exact path.
                    let status = Command::new("/usr/bin/mkfifo")
                        .args(["-m", "600"])
                        .arg(&owner.path)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::inherit())
                        .status()?;
                    if !status.success() {
                        return Err(io::Error::other("private output pipe unavailable"));
                    }
                    return Ok(owner);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::ErrorKind::AlreadyExists.into())
    }

    fn reader(&self) -> io::Result<File> {
        rustix::fs::openat(
            rustix::fs::CWD,
            &self.path,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map(File::from)
        .map_err(Into::into)
    }
}

impl Drop for PrivatePipe {
    fn drop(&mut self) {
        // Both names belong only to the directory this owner created with
        // create-new semantics, never a reused public temporary output file.
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_dir(&self.directory);
    }
}

#[cfg(target_os = "macos")]
pub(in crate::app) fn run_bundle(
    bundle: &Path,
    args: &[String],
    foreground: bool,
) -> Result<Finished, Error> {
    let owner = PrivatePipe::new().map_err(|_| Error::Start)?;
    let reader = owner.reader().map_err(|_| Error::Start)?;
    let child = Command::new("/usr/bin/open")
        .args(super::super::open_args(
            bundle,
            &owner.path,
            foreground,
            args,
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|_| Error::Start)?;
    follow(child, reader, Some(START_TIMEOUT), DRAIN_TIMEOUT)
}

#[cfg(test)]
#[path = "fifo_tests.rs"]
mod tests;
