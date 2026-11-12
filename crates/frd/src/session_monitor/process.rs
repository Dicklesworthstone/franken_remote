//! Blocking subprocess operations live on this one supervised foreign-call thread.
use super::{Configuration, Error, OCCUPIED, Shared, Status, now_ns, protocol};
use rustix::event::{PollFd, PollFlags};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(test)]
use std::thread;
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    time::Duration,
};

fn live(shared: &Shared, child: &mut Child) -> Result<(), Error> {
    if let Status::Stopped(error) = shared.status() {
        return Err(error);
    }
    if child.try_wait().map_err(|_| Error::Cleanup)?.is_some() {
        return Err(Error::ProcessExited);
    }
    Ok(())
}
fn wait(shared: &Shared, child: &mut Child, writing: bool, until: u64) -> Result<(), Error> {
    live(shared, child)?;
    let until = until.min(shared.deadline()?);
    let remaining = Duration::from_nanos(until.saturating_sub(now_ns()?));
    let command = if writing {
        PollFd::new(child.stdin.as_ref().ok_or(Error::Pipe)?, PollFlags::OUT)
    } else {
        PollFd::new(child.stdout.as_ref().ok_or(Error::Pipe)?, PollFlags::IN)
    };
    shared
        .native_wake
        .wait(command, None, remaining)
        .map_err(|_| Error::Pipe)?;
    // Readiness, a timeout or an interrupted poll cannot extend evidence or
    // override a concurrent stop. The original owner decides before more IPC.
    live(shared, child)
}

fn exchange(
    shared: &Shared,
    child: &mut Child,
    query: &[u8],
    answer: &mut [u8],
) -> Result<(), Error> {
    let sent = now_ns()?;
    let until = sent.checked_add(250_000_000).ok_or(Error::Clock)?;
    let mut written = 0;
    let mut read = 0;
    while written < query.len() || read < answer.len() {
        if let Status::Stopped(error) = shared.status() {
            return Err(error);
        }
        // Every individual IPC transaction is bounded, including initial setup.
        if now_ns()? >= until {
            return Err(Error::Pipe);
        }
        if written < query.len() {
            match child
                .stdin
                .as_mut()
                .ok_or(Error::Pipe)?
                .write(&query[written..])
            {
                Ok(0) => return Err(Error::Pipe),
                Ok(n) => written += n,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Error::Pipe),
            }
        }
        if written == query.len() && read < answer.len() {
            match child
                .stdout
                .as_mut()
                .ok_or(Error::Pipe)?
                .read(&mut answer[read..])
            {
                Ok(0) => return Err(Error::Pipe),
                Ok(n) => read += n,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Error::Pipe),
            }
        }
        if written != query.len() || read != answer.len() {
            wait(shared, child, written != query.len(), until)?;
        }
    }
    Ok(())
}
fn serve(configuration: &Configuration, shared: &Shared, child: &mut Child) -> Result<(), Error> {
    let stdin = child.stdin.as_ref().ok_or(Error::Pipe)?;
    fcntl_setfl(
        stdin,
        fcntl_getfl(stdin).map_err(|_| Error::Pipe)? | OFlags::NONBLOCK,
    )
    .map_err(|_| Error::Pipe)?;
    let stdout = child.stdout.as_ref().ok_or(Error::Pipe)?;
    fcntl_setfl(
        stdout,
        fcntl_getfl(stdout).map_err(|_| Error::Pipe)? | OFlags::NONBLOCK,
    )
    .map_err(|_| Error::Pipe)?;
    let mut entropy = [0; 16];
    getrandom::fill(&mut entropy).map_err(|_| Error::Spawn)?;
    let epoch = u128::from_be_bytes(entropy);
    exchange(
        shared,
        child,
        &configuration.selection.encode(epoch)?,
        &mut [],
    )?;
    let mut sequence = 1_u64;
    loop {
        let sent = now_ns()?;
        let mut bytes = [0; protocol::REPLY_BYTES];
        exchange(
            shared,
            child,
            &protocol::query(epoch, sequence)?,
            &mut bytes,
        )?;
        let reply = protocol::Reply::decode(&bytes, epoch, sequence)?;
        if matches!(
            reply.state,
            protocol::State::Opening | protocol::State::Active
        ) {
            live(shared, child)?;
        }
        shared.accept(reply, sent)?;
        sequence = sequence.checked_add(1).ok_or(Error::Protocol)?;
        // No queued heartbeats. During the idle interval, process death and local
        // cancellation are checked independently of the next native response.
        let next = sent.checked_add(50_000_000).ok_or(Error::Clock)?;
        while now_ns()? < next {
            live(shared, child)?;
            // The protocol has one reply per query. Extra bytes are not queued
            // for the next sequence, nor allowed to turn readiness into a spin.
            match child.stdout.as_mut().ok_or(Error::Pipe)?.read(&mut [0]) {
                Ok(0) => return Err(Error::Pipe),
                Ok(_) => return Err(Error::Protocol),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => return Err(Error::Pipe),
            }
            wait(shared, child, false, next)?;
        }
    }
}

fn open_image(path: &std::path::Path) -> Result<std::fs::File, Error> {
    // Check the object after a nonblocking, no-follow open. An O_RDONLY open
    // of a FIFO waits for a writer before metadata is available, preventing
    // the original monitor worker from ever reaching its cleanup path.
    let image = OpenOptions::new()
        .read(true)
        .custom_flags(
            i32::try_from((OFlags::NOFOLLOW | OFlags::NONBLOCK).bits())
                .map_err(|_| Error::Image)?,
        )
        .open(path)
        .map_err(|_| Error::Image)?;
    let metadata = image.metadata().map_err(|_| Error::Image)?;
    let uid = rustix::process::geteuid().as_raw();
    if !metadata.is_file()
        || ![0, uid].contains(&metadata.uid())
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err(Error::Image);
    }
    Ok(image)
}

#[cfg(target_os = "macos")]
fn native_image_path(
    image: &std::fs::File,
    path: &std::path::Path,
) -> Result<std::path::PathBuf, Error> {
    // Darwin has no /proc/self/fd executable path. Execute the canonical
    // installed path only after checking its opened inode and every parent.
    // Root and the selected desktop user are explicit trust boundaries, as
    // for the other native roles; this is not protection from that user.
    let canonical = path.canonicalize().map_err(|_| Error::Image)?;
    let selected = image.metadata().map_err(|_| Error::Image)?;
    let checked = open_image(&canonical)?
        .metadata()
        .map_err(|_| Error::Image)?;
    if selected.dev() != checked.dev() || selected.ino() != checked.ino() {
        return Err(Error::Image);
    }
    let uid = rustix::process::geteuid().as_raw();
    for parent in canonical.ancestors().skip(1) {
        let metadata = std::fs::symlink_metadata(parent).map_err(|_| Error::Image)?;
        if !metadata.is_dir() || ![0, uid].contains(&metadata.uid()) || metadata.mode() & 0o022 != 0
        {
            return Err(Error::Image);
        }
    }
    Ok(canonical)
}

pub(super) fn run(configuration: &Configuration, shared: &Shared) -> Result<(), Error> {
    // Linux executes the opened image, while macOS uses the checked package
    // path. No shell, PATH, environment or bearer arguments are inherited.
    let launch = || -> Result<Child, Error> {
        let image = open_image(&configuration.image)?;
        if let Status::Stopped(error) = shared.status() {
            return Err(error);
        }
        #[cfg(target_os = "linux")]
        let path = std::path::PathBuf::from(format!("/proc/self/fd/{}", image.as_raw_fd()));
        #[cfg(target_os = "macos")]
        let path = native_image_path(&image, &configuration.image)?;
        Command::new(path)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Error::Spawn)
    };
    let mut child = match launch() {
        Ok(child) => child,
        Err(error) => {
            shared.stop(error);
            OCCUPIED.store(false, Ordering::Release);
            return Ok(());
        }
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        serve(configuration, shared, &mut child)
    }));
    shared.stop(
        result
            .unwrap_or(Err(Error::Protocol))
            .err()
            .unwrap_or(Error::Stopped),
    );
    // This process only reads session evidence; killing it cannot be labelled as
    // held-input release or native media cleanup. Reap before allowing replacement.
    let _ = child.kill();
    let reaped = child.wait().map(|_| ()).map_err(|_| Error::Cleanup);
    if reaped.is_ok() {
        OCCUPIED.store(false, Ordering::Release);
    }
    reaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fifo_image_refuses_without_waiting_for_a_writer() {
        use std::sync::mpsc;

        let mut entropy = [0; 8];
        getrandom::fill(&mut entropy).unwrap();
        let path = std::env::temp_dir().join(format!(
            "fr-monitor-image-fifo-{}-{:x}",
            std::process::id(),
            u64::from_ne_bytes(entropy)
        ));
        // POSIX mkfifo(1): rustix has no FIFO constructor on Apple targets.
        let made = std::process::Command::new("mkfifo")
            .args(["-m", "600"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(made.success(), "mkfifo {}", path.display());
        let (reply, result) = mpsc::sync_channel(1);
        let task = thread::spawn(move || {
            let _ = reply.send(open_image(&path).map(|_| ()));
        });
        assert_eq!(
            result.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(Error::Image)
        );
        task.join().unwrap();
    }
}
