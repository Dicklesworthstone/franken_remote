//! Blocking subprocess operations live on this one supervised foreign-call thread.
use super::{Configuration, Error, OCCUPIED, Shared, Status, now_ns, protocol};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use std::{
    fs::OpenOptions,
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    process::{Child, Command, Stdio},
    sync::atomic::Ordering,
    thread,
    time::Duration,
};
const TURN: Duration = Duration::from_millis(5);

fn live(shared: &Shared, child: &mut Child) -> Result<(), Error> {
    if let Status::Stopped(error) = shared.status() {
        return Err(error);
    }
    if child.try_wait().map_err(|_| Error::Cleanup)?.is_some() {
        return Err(Error::ProcessExited);
    }
    Ok(())
}
fn exchange(
    shared: &Shared,
    child: &mut Child,
    query: &[u8],
    answer: &mut [u8],
) -> Result<(), Error> {
    let sent = now_ns()?;
    let mut written = 0;
    let mut read = 0;
    while written < query.len() || read < answer.len() {
        if let Status::Stopped(error) = shared.status() {
            return Err(error);
        }
        // Every individual IPC transaction is bounded, including initial setup.
        if now_ns()?.saturating_sub(sent) >= 250_000_000 {
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
            live(shared, child)?;
            thread::sleep(TURN);
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
        while now_ns()?.saturating_sub(sent) < 50_000_000 {
            live(shared, child)?;
            thread::sleep(TURN);
        }
    }
}

pub(super) fn run(configuration: &Configuration, shared: &Shared) -> Result<(), Error> {
    // Execute the opened ELF image, not a path that could be replaced after the
    // ownership check. No shell, scripts, PATH, environment or bearer arguments.
    let launch = || -> Result<Child, Error> {
        let image = OpenOptions::new()
            .read(true)
            .custom_flags(i32::try_from(OFlags::NOFOLLOW.bits()).map_err(|_| Error::Image)?)
            .open(&configuration.image)
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
        if let Status::Stopped(error) = shared.status() {
            return Err(error);
        }
        Command::new(format!("/proc/self/fd/{}", image.as_raw_fd()))
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
