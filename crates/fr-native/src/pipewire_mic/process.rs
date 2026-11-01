#![forbid(unsafe_code)]
//! Private device child for a parent-owned virtual microphone and sibling codec.
//! stdin is the parent command socket; stdout is the sibling codec socket;
//! stderr is a separate one-way EOF revoke gate. No child process is spawned.
//! The parent retains both OS child handles and owns every exit receipt.
use super::{
    Microphone,
    uplink::{ReceiveResult, Uplink},
};
use crate::opus::process::socket::SocketDecoder;
use fr_media::worker::microphone::{
    self as ipc, Configuration, Header, Incoming, Kind, Outgoing, Record, Snapshot, Status,
};
use fr_wire::audio;
use std::{
    io::Read,
    os::{fd::AsFd, unix::net::UnixStream},
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Parent,
    Protocol,
    Clock,
    Expired,
    Permission,
    Native,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "microphone-device-worker: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<ipc::Error> for Error {
    fn from(error: ipc::Error) -> Self {
        match error {
            ipc::Error::Clock => Self::Clock,
            ipc::Error::Expired => Self::Expired,
            _ => Self::Protocol,
        }
    }
}

struct Guard {
    channel: UnixStream,
    authorization_ns: u64,
    denied: bool,
}
impl Guard {
    fn permitted(&mut self, cap: u64) -> bool {
        if self.denied
            || crate::clock::monotonic_ns().is_none_or(|now| now >= self.authorization_ns.min(cap))
        {
            self.denied = true;
            return false;
        }
        let mut byte = [0];
        if !matches!(self.channel.read(&mut byte), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
        {
            self.denied = true;
        }
        !self.denied
    }
    fn renew(&mut self, request: Header) -> Result<(), Error> {
        // The old capability must still be live. A later valid request can
        // refresh future admission, never revive an expired original channel.
        if !self.permitted(u64::MAX) {
            return Err(Error::Permission);
        }
        let predecessor = self.authorization_ns;
        request.live(crate::clock::monotonic_ns().ok_or(Error::Clock)?)?;
        self.authorization_ns = request.authorization_ns;
        // Installation itself must still precede the old cap, including a
        // scheduling pause between validation and this assignment.
        if !self.permitted(request.expires_ns.min(predecessor)) {
            return Err(Error::Permission);
        }
        Ok(())
    }
}

/// Only the installed standalone device image calls this. Native initialization
/// follows kernel parent-death binding and bounded private configuration checks.
pub fn run(parent: u32) -> Result<(), Error> {
    crate::bind_parent(parent).map_err(|_| Error::Parent)?;
    let command = std::io::stdin()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|_| Error::Protocol)?;
    let codec = std::io::stdout()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|_| Error::Protocol)?;
    let revoke = std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map_err(|_| Error::Protocol)?;
    let command = UnixStream::from(command);
    let codec = UnixStream::from(codec);
    let revoke = UnixStream::from(revoke);
    for socket in [&command, &codec, &revoke] {
        socket.peer_addr().map_err(|_| Error::Protocol)?;
        socket.set_nonblocking(true).map_err(|_| Error::Protocol)?;
    }
    serve(command, codec, revoke)
}

fn read_first(command: &mut UnixStream, guard: &mut Guard) -> Result<Record, Error> {
    let until = Instant::now()
        .checked_add(Duration::from_nanos(ipc::SETUP_NS))
        .ok_or(Error::Clock)?;
    let mut incoming = Incoming::default();
    loop {
        if Instant::now() >= until || !guard.permitted(u64::MAX) {
            return Err(Error::Permission);
        }
        if let Some(header) = incoming.header() {
            header.live(crate::clock::monotonic_ns().ok_or(Error::Clock)?)?;
        }
        if let Some(record) = incoming.poll(command, false)? {
            return Ok(record);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn write_reply(command: &mut UnixStream, guard: &mut Guard, record: &Record) -> Result<(), Error> {
    let mut outgoing = Outgoing::new(record, true)?;
    loop {
        if !guard.permitted(record.header.expires_ns) {
            return Err(Error::Permission);
        }
        if outgoing.poll(command)? {
            if !guard.permitted(record.header.expires_ns) {
                return Err(Error::Permission);
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn snapshot(uplink: &Uplink<SocketDecoder>, admitted_packet: bool) -> Snapshot {
    let stats = uplink.statistics();
    Snapshot {
        node_id: stats.node_id,
        failure_code: stats.failure.code,
        submitted_frames: stats.submitted_frames,
        silent_frames: stats.silent_frames,
        expired_frames: stats.expired_frames,
        queued_frames: stats.queued_frames,
        queued_packets: u16::try_from(uplink.queued_packets()).unwrap_or(u16::MAX),
        admitted_packet,
    }
}
fn configured(
    options: &Configuration,
    binding: u32,
) -> Result<[u8; audio::AUDIO_CONFIGURED_RECORD_BYTES], Error> {
    let config = options.validate()?;
    let mut record = [0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
    audio::encode_configured(
        &audio::AudioConfigured {
            direction: config.direction(),
            generation: config.generation(),
            accepted: true,
            actual_channels: config.channels(),
            actual_sample_rate: config.sample_rate(),
            actual_frame_duration_ms: config.frame_duration_ms(),
        },
        binding,
        &mut record,
    )
    .map_err(|_| Error::Protocol)?;
    Ok(record)
}
fn serve(mut command: UnixStream, codec: UnixStream, revoke: UnixStream) -> Result<(), Error> {
    let initial = crate::clock::monotonic_ns()
        .and_then(|now| now.checked_add(ipc::SETUP_NS))
        .ok_or(Error::Clock)?;
    let mut guard = Guard {
        channel: revoke,
        authorization_ns: initial,
        denied: false,
    };
    let first = read_first(&mut command, &mut guard)?;
    first
        .header
        .live(crate::clock::monotonic_ns().ok_or(Error::Clock)?)?;
    let options = Configuration::decode(first.header, &first.body)?;
    guard.authorization_ns = first.header.authorization_ns;
    let initialized = connect(&options, first.header, codec, &mut guard);
    let mut uplink = match initialized {
        Ok(uplink) => uplink,
        Err(error) => {
            let record = Record {
                header: first.header.reply(Status::Native, 0)?,
                body: Vec::new(),
            };
            let _ = write_reply(&mut command, &mut guard, &record);
            return Err(error);
        }
    };
    let positive = configured(&options, first.header.binding)?;
    let mut body = snapshot(&uplink, false).encode().to_vec();
    body.extend_from_slice(&positive);
    // This private reply proves actual source+codec readiness, not network ACK.
    write_reply(
        &mut command,
        &mut guard,
        &Record {
            header: first.header.reply(Status::Ok, body.len())?,
            body,
        },
    )?;
    let mut incoming = Incoming::default();
    let mut outgoing: Option<(Outgoing, u64)> = None;
    let mut serial = first.header.serial;
    let mut committed = false;
    let mut partial_until: Option<Instant> = None;
    loop {
        let pending_cap = outgoing.as_ref().map_or(u64::MAX, |(_, cap)| *cap);
        if !guard.permitted(pending_cap) {
            return Err(Error::Permission);
        }
        // A pending renewal/commit reply retains its predecessor/exchange cap
        // through the very next native callback, before any new-cap output.
        uplink
            .service(|| guard.permitted(pending_cap))
            .map_err(|_| Error::Native)?;
        if let Some((reply, _)) = outgoing.as_mut() {
            reply
                .header()
                .live(crate::clock::monotonic_ns().ok_or(Error::Clock)?)?;
            if reply.poll(&mut command)? {
                if !guard.permitted(pending_cap) {
                    return Err(Error::Permission);
                }
                outgoing = None;
            }
        } else {
            if partial_until.is_some_and(|until| Instant::now() >= until) {
                return Err(Error::Expired);
            }
            if let Some(header) = incoming.header() {
                header.live(crate::clock::monotonic_ns().ok_or(Error::Clock)?)?;
            }
            let request = incoming.poll(&mut command, false)?;
            if incoming.is_partial() && partial_until.is_none() {
                partial_until =
                    Instant::now().checked_add(Duration::from_nanos(ipc::MAX_RECORD_NS));
                if partial_until.is_none() {
                    return Err(Error::Clock);
                }
            }
            if let Some(request) = request {
                partial_until = None;
                serial = serial.checked_add(1).ok_or(Error::Protocol)?;
                if request.header.serial != serial
                    || request.header.binding != first.header.binding
                    || request.header.generation != first.header.generation
                    || request.header.kind == Kind::Configure
                    || request.body.len() > options.packet_maximum
                {
                    return Err(Error::Protocol);
                }
                let Some(reply) =
                    apply_request(&mut uplink, &mut guard, &request, &positive, &mut committed)?
                else {
                    return Ok(());
                };
                outgoing = Some(reply);
            }
        }
        // No runtime lives here. One fixed native/I/O turn services the actual
        // graph; the parent independently watches authority, stalls and exit.
        std::thread::sleep(Duration::from_micros(500));
    }
}

fn connect(
    options: &Configuration,
    first: Header,
    codec: UnixStream,
    guard: &mut Guard,
) -> Result<Uplink<SocketDecoder>, Error> {
    let device = Microphone::connect(Path::new(&options.server), options.validate()?, || {
        guard.permitted(first.expires_ns)
    })
    .map_err(|_| Error::Native)?;
    let mut uplink = Uplink::from_socket(first.binding, options.offer, device, codec, || {
        guard.permitted(first.expires_ns)
    })
    .map_err(|_| Error::Native)?;
    while !uplink
        .poll_ready(|| guard.permitted(first.expires_ns))
        .map_err(|_| Error::Native)?
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(uplink)
}
fn apply_request(
    uplink: &mut Uplink<SocketDecoder>,
    guard: &mut Guard,
    request: &Record,
    positive: &[u8; audio::AUDIO_CONFIGURED_RECORD_BYTES],
    committed: &mut bool,
) -> Result<Option<(Outgoing, u64)>, Error> {
    let operation_cap = request.header.expires_ns.min(guard.authorization_ns);
    guard.renew(request.header)?;
    let admitted = match request.header.kind {
        Kind::Commit if !*committed => {
            // The parent sends Commit only after original-route admission
            // of these exact bytes. No packet is accepted before it.
            uplink
                .acknowledge(
                    || guard.permitted(operation_cap),
                    |record| {
                        if record == positive { Ok(()) } else { Err(()) }
                    },
                )
                .map_err(|_| Error::Native)?;
            *committed = true;
            false
        }
        Kind::Packet if *committed => {
            let arrived =
                crate::clock::local_instant(request.header.arrived_ns).ok_or(Error::Clock)?;
            let expires =
                crate::clock::local_instant(request.header.expires_ns).ok_or(Error::Clock)?;
            let outcome = uplink
                .receive_record_until(&request.body, arrived, expires, || {
                    guard.permitted(operation_cap)
                })
                .map_err(|_| Error::Native)?;
            if matches!(outcome, ReceiveResult::Stopped(_)) {
                return Ok(None);
            }
            matches!(outcome, ReceiveResult::Queued)
        }
        Kind::Health => false,
        _ => return Err(Error::Protocol),
    };
    let body = snapshot(uplink, admitted).encode().to_vec();
    Ok(Some((
        Outgoing::new(
            &Record {
                header: request.header.reply(Status::Ok, body.len())?,
                body,
            },
            true,
        )?,
        operation_cap,
    )))
}
