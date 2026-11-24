//! One bounded native supervisor owns process creation, IPC and actual reaping.
//! Mutexes protect only mailbox transitions, never foreign/OS work or waits.
use super::{
    Arc, AudioMediaError as Error, AudioStreamConfig, CodecLimits, Duration, Instant, Ordering,
    PathBuf, Shared, Slot, exchange,
};
use std::{
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
};
struct ChildOwner {
    child: Child,
    shared: Arc<Shared>,
    slot: Option<Slot>,
}
impl Drop for ChildOwner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        if self.child.wait().is_ok() {
            self.shared.pid.store(0, Ordering::Release);
            self.shared.retired.store(true, Ordering::Release);
        } else if let Some(slot) = self.slot.take() {
            std::mem::forget(slot);
        }
        // An unproved reap permanently consumes this bounded admission slot.
    }
}
struct Exchange<'a> {
    socket: UnixStream,
    shared: &'a Shared,
    until: Instant,
}
impl Exchange<'_> {
    fn live(&self) -> std::io::Result<Duration> {
        if self
            .shared
            .mail
            .lock()
            .map_err(|_| std::io::ErrorKind::Other)?
            .stop
        {
            return Err(std::io::ErrorKind::ConnectionAborted.into());
        }
        self.until
            .checked_duration_since(Instant::now())
            .filter(|v| !v.is_zero())
            .map(|d| d.min(Duration::from_millis(10)))
            .ok_or_else(|| std::io::ErrorKind::TimedOut.into())
    }
}
impl Read for Exchange<'_> {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let remaining = self.live()?;
            self.socket.set_read_timeout(Some(remaining))?;
            match self.socket.read(b) {
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                v => return v,
            }
        }
    }
}
impl Write for Exchange<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        loop {
            let remaining = self.live()?;
            self.socket.set_write_timeout(Some(remaining))?;
            match self.socket.write(b) {
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                v => return v,
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl exchange::Endpoint for Exchange<'_> {
    fn set_deadline(&mut self, until: Instant) {
        self.until = until;
    }
}
pub(super) fn run(
    image: PathBuf,
    c: AudioStreamConfig,
    l: CodecLimits,
    until: Instant,
    shared: &Arc<Shared>,
    slot: Slot,
) {
    let result = serve(image, c, l, until, shared, slot);
    if let Err(error) = result {
        shared.reply(Err(error));
    }
    // serve either never spawned or finished its ChildOwner (including wait).
    // ChildOwner itself deliberately withholds completion on unproved reap.
}
fn serve(
    image: PathBuf,
    c: AudioStreamConfig,
    l: CodecLimits,
    until: Instant,
    shared: &Arc<Shared>,
    slot: Slot,
) -> Result<(), Error> {
    let (socket, child_socket) = UnixStream::pair().map_err(|_| {
        shared.retired.store(true, Ordering::Release);
        Error::Fatal
    })?;
    let stdout = child_socket.try_clone().map_err(|_| {
        shared.retired.store(true, Ordering::Release);
        Error::Fatal
    })?;
    let stdin: OwnedFd = child_socket.into();
    let stdout: OwnedFd = stdout.into();
    let child = Command::new(image)
        .env_clear()
        .args(["--parent-pid", &std::process::id().to_string()])
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|_| {
            shared.retired.store(true, Ordering::Release);
            Error::Fatal
        })?;
    shared.pid.store(child.id(), Ordering::Release);
    let _owner = ChildOwner {
        child,
        shared: shared.clone(),
        slot: Some(slot),
    };
    let mut exchange = Exchange {
        socket,
        shared,
        until,
    };
    exchange::serve(c, l, shared, &mut exchange)
}
