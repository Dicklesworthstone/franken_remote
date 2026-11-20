//! One bounded supervisor owns native process creation, IPC and actual reaping.
//! No application callback or OS wait runs with the mailbox mutex held.
use super::{
    Arc, CaptureOptions, Deadline, Error, MONITOR_TURN, Mailbox, Ordering, Packets, PathBuf, Reply,
    Request, Shared, Slot,
    protocol::{self, CONFIGURE, Header, PULL, RENEW},
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
    deadline: Deadline,
}
impl Exchange<'_> {
    fn live(&self) -> std::io::Result<Duration> {
        self.shared
            .check_until(Some(self.deadline))
            .map_err(|_| std::io::ErrorKind::PermissionDenied)?;
        self.deadline
            .narrow(
                self.shared
                    .authorization()
                    .map_err(|_| std::io::ErrorKind::PermissionDenied)?,
            )
            .remaining()
            .map(|remaining| remaining.min(MONITOR_TURN))
            .map_err(|error| {
                self.shared.stop(error);
                std::io::ErrorKind::TimedOut.into()
            })
    }
}
impl Read for Exchange<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.socket.set_read_timeout(Some(self.live()?))?;
            self.live()?;
            let result = self.socket.read(bytes);
            self.live()?;
            match result {
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
            self.live()?;
            let result = self.socket.write(bytes);
            self.live()?;
            match result {
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
        self.live().map(|_| ())
    }
}

pub(super) fn run(
    image: PathBuf,
    options: &CaptureOptions,
    deadline: Deadline,
    shared: &Arc<Shared>,
    slot: Slot,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        serve(image, options, deadline, shared, slot)
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
    deadline: Deadline,
    shared: &Arc<Shared>,
    slot: Slot,
) -> Result<(), Error> {
    shared.check_until(Some(deadline))?;
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
    shared.check_until(Some(deadline))?;
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
        deadline,
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
    exchange.shared.check_until(Some(exchange.deadline))?;
    Ok((header, body))
}

fn pending_deadline(mail: &Mailbox, exchange: Deadline) -> Option<Deadline> {
    match mail.reply.as_ref() {
        Some(Reply::Ready | Reply::Renewed) => Some(exchange),
        Some(Reply::Packets(packets)) => Some(packets.deadline()),
        None => mail.request.as_ref().map(Request::deadline),
    }
}

fn next_request(shared: &Shared, exchange: Deadline) -> Result<Option<Request>, Error> {
    shared.check()?;
    let current = shared.authorization()?;
    let mut mail = shared.mail.lock().map_err(|_| Error::Stopped)?;
    if shared.stopped.load(Ordering::Acquire) {
        return Err(mail.error.unwrap_or(Error::Stopped));
    }
    let deadline = pending_deadline(&mail, exchange).map_or(current, |cap| current.narrow(cap));
    let timeout = deadline.remaining()?.min(MONITOR_TURN);
    if let Some(request) = mail.request.take() {
        return Ok(Some(request));
    }
    let (mail, _) = shared
        .wake
        .wait_timeout(mail, timeout)
        .map_err(|_| Error::Stopped)?;
    // Reinspect the mailbox after the wait: a result consumed while we slept
    // no longer owns its slot, whereas a newly enqueued request keeps its cap.
    pending_deadline(&mail, exchange)
        .map_or(current, |cap| current.narrow(cap))
        .remaining()?;
    drop(mail);
    shared.check()?;
    Ok(None)
}

fn session(
    options: &CaptureOptions,
    exchange: &mut Exchange<'_>,
    child: &mut Child,
) -> Result<(), Error> {
    let body = protocol::configuration(options)?;
    let shared = exchange.shared;
    let original = shared.authorization()?;
    exchange.deadline = exchange.deadline.narrow(original);
    shared.check_until(Some(exchange.deadline))?;
    let mut header = Header {
        kind: CONFIGURE,
        serial: 1,
        generation: options.configuration.generation().as_raw(),
        authorization_ns: original.ns,
        expires_ns: exchange.deadline.ns,
        bytes: u32::try_from(body.len()).map_err(|_| Error::Protocol)?,
        error: None,
    };
    header.write(exchange, false, &body)?;
    let (ready, echoed) = reply(exchange, header)?;
    if ready.expires_ns != header.expires_ns || echoed != body {
        return Err(Error::Protocol);
    }
    shared.reply(Reply::Ready, exchange.deadline)?;
    loop {
        // Observe the original child even without another caller request. A
        // successful try_wait is retained by Child's later final wait.
        if child.try_wait().map_err(|_| Error::Native)?.is_some() {
            return Err(Error::Native);
        }
        let request = next_request(shared, exchange.deadline)?;
        let Some(request) = request else { continue };
        let current = shared.authorization()?;
        let (until, renewed) = match request {
            Request::Pull(until) => (until, None),
            Request::Renew {
                authorization,
                expires,
            } => (expires, Some(authorization)),
        };
        exchange.deadline = until.narrow(current);
        shared.check_until(Some(exchange.deadline))?;
        header.kind = if renewed.is_some() { RENEW } else { PULL };
        header.serial = header.serial.checked_add(1).ok_or(Error::Protocol)?;
        header.authorization_ns = renewed.map_or(current.ns, |authorization| authorization.ns);
        header.expires_ns = exchange.deadline.ns.min(header.authorization_ns);
        header.bytes = 0;
        header.write(exchange, false, &[])?;
        let (received, body) = reply(exchange, header)?;
        if let Some(authorization) = renewed {
            if received.expires_ns != header.expires_ns || !body.is_empty() {
                return Err(Error::Protocol);
            }
            shared.install_authorization(current.ns, authorization, exchange.deadline)?;
            shared.reply(Reply::Renewed, exchange.deadline)?;
            continue;
        }
        let expires = crate::clock::local_instant(received.expires_ns)
            .ok_or(Error::Clock)?
            .min(until.until);
        let deadline = Deadline {
            until: expires,
            ns: received.expires_ns,
        }
        .narrow(exchange.deadline);
        shared.check_until(Some(deadline))?;
        let (units, counters) = protocol::read_packets(&body, options)?;
        shared.reply(
            Reply::Packets(Packets {
                units,
                counters,
                expires: deadline.until,
                expires_ns: deadline.ns,
            }),
            deadline,
        )?;
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn raw_expired_exchange_never_reads_or_writes_even_with_live_uptime() {
        for reading in [false, true] {
            let shared = super::super::deadline_tests::shared(|| {
                panic!("expired exchange cannot consult the permission monitor")
            });
            let (socket, mut peer) = UnixStream::pair().unwrap();
            let mut retained = socket.try_clone().unwrap();
            let mut exchange = Exchange {
                socket,
                shared: &shared,
                deadline: Deadline {
                    until: Instant::now() + Duration::from_secs(3600),
                    ns: 1,
                },
            };
            if reading {
                peer.write_all(b"x").unwrap();
                assert!(exchange.read(&mut [0]).is_err());
                let mut original = [0];
                retained.read_exact(&mut original).unwrap();
                assert_eq!(
                    original, *b"x",
                    "expired read must leave original bytes untouched"
                );
            } else {
                assert!(exchange.write(b"x").is_err());
                peer.set_nonblocking(true).unwrap();
                assert_eq!(
                    peer.read(&mut [0]).unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
            }
            assert_eq!(shared.error(), Error::Expired);
        }
    }

    #[test]
    fn queued_request_and_uncollected_ack_retain_their_original_native_caps() {
        let shared = super::super::deadline_tests::shared(|| true);
        let current = shared.authorization().unwrap();
        let expired = Deadline {
            until: current.until,
            ns: 1,
        };
        {
            let mut mail = shared.mail.lock().unwrap();
            mail.request = Some(Request::Pull(expired));
            assert_eq!(pending_deadline(&mail, current), Some(expired));
        }
        assert!(matches!(
            next_request(&shared, current),
            Err(Error::Expired)
        ));
        let mut mail = shared.mail.lock().unwrap();
        mail.request = None;
        mail.reply = Some(Reply::Renewed);
        assert_eq!(pending_deadline(&mail, expired), Some(expired));
        drop(mail);
        assert!(matches!(
            next_request(&shared, expired),
            Err(Error::Expired)
        ));
        assert_eq!(shared.authorization().unwrap(), current);
    }
}
