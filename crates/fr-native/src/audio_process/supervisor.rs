//! One bounded supervisor owns native process creation, IPC and actual reaping.
//! No application callback or OS wait runs with the mailbox mutex held.
use super::{
    Arc, CaptureOptions, Error, Instant, MONITOR_TURN, Ordering, Packets, PathBuf, Reply, Shared,
    Slot,
    protocol::{self, CONFIGURE, Header, PULL},
};
use std::{
    io::{Read, Write},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    time::Duration,
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
            // An unproved reap permanently consumes this bounded admission slot.
            std::mem::forget(slot);
        }
    }
}

struct Exchange<'a> {
    socket: UnixStream,
    shared: &'a Shared,
    until: Instant,
}
impl Exchange<'_> {
    fn live(&self) -> std::io::Result<Duration> {
        self.shared
            .check()
            .map_err(|_| std::io::ErrorKind::PermissionDenied)?;
        self.until
            .min(self.shared.authorized_until)
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .map(|remaining| remaining.min(MONITOR_TURN))
            .ok_or_else(|| {
                self.shared.stop(Error::Expired);
                std::io::ErrorKind::TimedOut.into()
            })
    }
}
impl Read for Exchange<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.socket.set_read_timeout(Some(self.live()?))?;
            match self.socket.read(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                result => return result,
            }
        }
    }
}
impl Write for Exchange<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        loop {
            self.socket.set_write_timeout(Some(self.live()?))?;
            match self.socket.write(bytes) {
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) => {}
                result => return result,
            }
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn run(
    image: PathBuf,
    options: CaptureOptions,
    until: Instant,
    shared: &Arc<Shared>,
    slot: Slot,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        serve(image, &options, until, shared, slot)
    }))
    .unwrap_or(Err(Error::Native));
    if let Err(error) = result {
        shared.stop(error);
    }
    // No child was created, or ChildOwner has positively completed its wait.
    // A failed wait retains both the PID and the original admission slot.
    if shared.pid.load(Ordering::Acquire) == 0 {
        shared.retired.store(true, Ordering::Release);
    }
}

fn serve(
    image: PathBuf,
    options: &CaptureOptions,
    until: Instant,
    shared: &Arc<Shared>,
    slot: Slot,
) -> Result<(), Error> {
    shared.check()?;
    let (socket, child_socket) = UnixStream::pair().map_err(|_| Error::Spawn)?;
    let stdout = child_socket.try_clone().map_err(|_| Error::Spawn)?;
    let (revoke, child_guard) = UnixStream::pair().map_err(|_| Error::Spawn)?;
    {
        let mut mail = shared.mail.lock().map_err(|_| Error::Stopped)?;
        if shared.stopped.load(Ordering::Acquire) {
            return Err(mail.error.unwrap_or(Error::Stopped));
        }
        mail.revoke = Some(revoke);
    }
    shared.check()?;
    let stdin: OwnedFd = child_socket.into();
    let stdout: OwnedFd = stdout.into();
    let guard: OwnedFd = child_guard.into();
    let child = Command::new(image)
        .env_clear()
        .args(["--parent-pid", &std::process::id().to_string()])
        .stdin(Stdio::from(stdin))
        .stdout(Stdio::from(stdout))
        // Descriptor 2 is a dedicated terminal revocation channel, not logging.
        .stderr(Stdio::from(guard))
        .process_group(0)
        .spawn()
        .map_err(|_| Error::Spawn)?;
    shared.pid.store(child.id(), Ordering::Release);
    let mut owner = ChildOwner {
        child,
        shared: shared.clone(),
        slot: Some(slot),
    };
    let mut exchange = Exchange {
        socket,
        shared,
        until,
    };
    let result = session(options, &mut exchange, &mut owner.child);
    shared.stop(result.as_ref().err().copied().unwrap_or(Error::Stopped));
    drop(owner);
    result
}

fn reply(exchange: &mut Exchange<'_>, request: Header) -> Result<(Header, Vec<u8>), Error> {
    let header = Header::read(exchange, true)?;
    if !header.matches_reply(request) {
        return Err(Error::Protocol);
    }
    if let Some(error) = header.error {
        return Err(error);
    }
    let body = protocol::body(exchange, header)?;
    exchange.shared.check()?;
    if Instant::now() >= exchange.until {
        return Err(Error::Expired);
    }
    Ok((header, body))
}

fn session(
    options: &CaptureOptions,
    exchange: &mut Exchange<'_>,
    child: &mut Child,
) -> Result<(), Error> {
    let body = protocol::configuration(options)?;
    let shared = exchange.shared;
    let mut header = Header {
        kind: CONFIGURE,
        serial: 1,
        generation: options.configuration.generation().as_raw(),
        authorization_ns: shared.authorization_ns,
        expires_ns: crate::clock::stamp_ns(exchange.until)
            .ok_or(Error::Clock)?
            .min(shared.authorization_ns),
        bytes: u32::try_from(body.len()).map_err(|_| Error::Protocol)?,
        error: None,
    };
    header.write(exchange, false, &body)?;
    let (ready, echoed) = reply(exchange, header)?;
    if ready.expires_ns != header.expires_ns || echoed != body {
        return Err(Error::Protocol);
    }
    shared.reply(Reply::Ready)?;
    loop {
        // A child can fail between pulls. Observe and reap its original PID
        // even when no caller is polling or submitting another IPC request.
        // Child::wait below retains this cached successful wait result.
        if child.try_wait().map_err(|_| Error::Native)?.is_some() {
            return Err(Error::Native);
        }
        // Idle does not mean authorized forever. The original durable monitor
        // is checked at least every 10 ms, independent of caller service turns.
        shared.check()?;
        let until = {
            let mut mail = shared.mail.lock().map_err(|_| Error::Stopped)?;
            if shared.stopped.load(Ordering::Acquire) {
                return Err(mail.error.unwrap_or(Error::Stopped));
            }
            let reply_until = match mail.reply.as_ref() {
                Some(Reply::Ready) => Some(exchange.until),
                Some(Reply::Packets(packets)) => Some(packets.expires),
                None => None,
            };
            if reply_until.is_some_and(|until| Instant::now() >= until) {
                return Err(Error::Expired);
            }
            if let Some(request) = mail.request.take() {
                Some(request)
            } else {
                let timeout = shared
                    .authorized_until
                    .min(reply_until.unwrap_or(shared.authorized_until))
                    .saturating_duration_since(Instant::now())
                    .min(MONITOR_TURN);
                drop(
                    shared
                        .wake
                        .wait_timeout(mail, timeout)
                        .map_err(|_| Error::Stopped)?,
                );
                None
            }
        };
        let Some(until) = until else { continue };
        shared.check()?;
        exchange.until = until;
        header.kind = PULL;
        header.serial = header.serial.checked_add(1).ok_or(Error::Protocol)?;
        header.expires_ns = crate::clock::stamp_ns(until)
            .ok_or(Error::Clock)?
            .min(shared.authorization_ns);
        header.bytes = 0;
        header.write(exchange, false, &[])?;
        let (received, body) = reply(exchange, header)?;
        let expires = crate::clock::local_instant(received.expires_ns)
            .ok_or(Error::Clock)?
            .min(until);
        if Instant::now() >= expires {
            return Err(Error::Expired);
        }
        let (units, counters) = protocol::read_packets(&body, options)?;
        shared.reply(Reply::Packets(Packets {
            units,
            counters,
            expires,
        }))?;
    }
}
