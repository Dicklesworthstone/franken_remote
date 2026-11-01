#![forbid(unsafe_code)]
//! Host microphone supervision with two actual, independently reaped children.
//!
//! The daemon owns the original controller proof, transport binding and local
//! microphone consent. One bounded foreign supervisor thread owns the device
//! and sandboxed codec as siblings; no native audio library is linked here.
//! The worker's positive configuration is admitted to the original authenticated
//! route before a separate Commit can arm output. Public microphone discovery
//! and approval UI remain the containing coordinator's responsibility.
use fr_core::{audio::AudioDirection, input_submission::Refusal, time::HostInstant};
use fr_media::{
    audio::authority::HostController,
    worker::microphone::{
        self as ipc, Configuration, Header, Incoming, Kind, Outgoing, Record, Snapshot, Status,
    },
};
use fr_wire::{
    audio,
    negotiation::{ControlBinding, Selection},
};
use std::{
    io::{self, Read},
    net::Shutdown,
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_OWNERS: usize = 4;
const POLL: Duration = Duration::from_millis(1);
const HEALTH: Duration = Duration::from_millis(10);
static OWNERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Permission,
    NotSelected,
    Binding,
    Configuration,
    Clock,
    Authority(Refusal),
    Limit,
    Spawn,
    Protocol,
    Timeout,
    Stopped,
    NotReady,
    Busy,
    AcknowledgementUnknown,
    Native(Status),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host-microphone-process: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<ipc::Error> for Error {
    fn from(error: ipc::Error) -> Self {
        match error {
            ipc::Error::Clock => Self::Clock,
            ipc::Error::Expired => Self::Timeout,
            ipc::Error::Limit => Self::Limit,
            _ => Self::Protocol,
        }
    }
}

/// Metadata of the completed original authenticated attachment. The containing
/// coordinator proves route ownership; these public numbers cannot grant it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    pub parent: ControlBinding,
    pub id: u32,
    pub packet_maximum: usize,
}

/// Locally installed absolute images and explicitly selected `PipeWire` socket.
/// Never derive paths, arguments or environment from a remote message.
pub struct Launch {
    device: PathBuf,
    codec: PathBuf,
    server: String,
}
impl std::fmt::Debug for Launch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostMicrophoneLaunch")
    }
}
impl Launch {
    pub fn new(device: &Path, codec: &Path, server: &Path) -> Result<Self, Error> {
        if [device, codec, server].into_iter().any(|path| {
            !path.is_absolute()
                || path
                    .components()
                    .any(|component| matches!(component, Component::ParentDir))
        }) {
            return Err(Error::Configuration);
        }
        Ok(Self {
            device: device.into(),
            codec: codec.into(),
            server: server.to_str().ok_or(Error::Configuration)?.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Ready,
    Committing,
    Running,
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Queued,
    Ignored,
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketOutcome {
    Admitted,
    Ignored,
}

/// An actual OS wait receipt, never inferred from a socket closing or a fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildExit {
    NotStarted,
    Running,
    Reaped,
    Unresolved,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitReceipts {
    pub device: ChildExit,
    pub codec: ChildExit,
}
/// Original OS identities for diagnostics; they never grant stop/reaping
/// authority. The supervisor retains and acts on the actual Child handles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChildIds {
    pub device: u32,
    pub codec: u32,
}
fn exit_status(value: u8) -> ChildExit {
    match value {
        0 => ChildExit::NotStarted,
        1 => ChildExit::Running,
        2 => ChildExit::Reaped,
        _ => ChildExit::Unresolved,
    }
}

#[derive(Clone, Copy)]
struct Clock {
    origin: Instant,
    host: HostInstant,
}
impl Clock {
    fn bind(sample: impl FnOnce() -> HostInstant) -> Result<Self, Error> {
        let origin = Instant::now();
        let host = catch_unwind(AssertUnwindSafe(sample)).map_err(|_| Error::Clock)?;
        Ok(Self { origin, host })
    }
    fn now(self) -> Result<HostInstant, Error> {
        let elapsed = Instant::now()
            .checked_duration_since(self.origin)
            .ok_or(Error::Clock)?;
        let micros = u64::try_from(elapsed.as_nanos().div_ceil(1000)).map_err(|_| Error::Clock)?;
        self.host
            .as_micros()
            .checked_add(micros)
            .map(HostInstant::from_micros)
            .ok_or(Error::Clock)
    }
}
struct Packet {
    body: Vec<u8>,
    arrived_ns: u64,
    expires_ns: u64,
}
impl Drop for Packet {
    fn drop(&mut self) {
        self.body.fill(0);
    }
}
enum Job {
    Commit,
    Packet(Packet),
}
struct Mail {
    state: State,
    job: Option<Job>,
    packet_busy: bool,
    packet_reply: Option<PacketOutcome>,
    snapshot: Option<Snapshot>,
    positive: Option<[u8; audio::AUDIO_CONFIGURED_RECORD_BYTES]>,
    error: Option<Error>,
}
struct Shared {
    channel: Channel,
    options: Configuration,
    controller: HostController,
    clock: Clock,
    stopped: AtomicBool,
    revoke: Mutex<Option<Arc<UnixStream>>>,
    mail: Mutex<Mail>,
    wake: Condvar,
    device_exit: AtomicU8,
    codec_exit: AtomicU8,
    device_pid: AtomicU32,
    codec_pid: AtomicU32,
    complete: AtomicBool,
}
impl Shared {
    fn lock(&self) -> MutexGuard<'_, Mail> {
        self.mail.lock().unwrap_or_else(PoisonError::into_inner)
    }
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let socket = self
            .revoke
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(socket) = socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        let mut mail = self.lock();
        mail.state = State::Stopped;
        mail.job = None;
        mail.packet_reply = None;
        mail.positive = None;
        drop(mail);
        self.wake.notify_all();
    }
    fn fail(&self, error: Error) {
        let mut mail = self.lock();
        if mail.error.is_none() {
            mail.error = Some(error);
        }
        drop(mail);
        self.stop();
    }
    fn check(&self) -> Result<(u64, u64), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Stopped);
        }
        let checked = (|| {
            // Sample raw time before the conservatively extrapolated original
            // host clock; any sampling gap shortens, rather than renews, the cap.
            let raw = monotonic_ns()?;
            let now = self.clock.now()?;
            let until = self.controller.deadline(now).map_err(Error::Authority)?;
            let cap = fr_core::input_submission::process::not_after_ns(raw, now, until)
                .ok_or(Error::Clock)?;
            if self.stopped.load(Ordering::Acquire) {
                return Err(Error::Stopped);
            }
            Ok((
                raw,
                cap.min(raw.checked_add(ipc::MAX_AUTHORITY_NS).ok_or(Error::Clock)?),
            ))
        })();
        if let Err(error) = checked {
            self.fail(error);
        }
        checked
    }
    fn bind_revoke(&self, socket: Arc<UnixStream>) -> Result<(), Error> {
        let mut installed = self.revoke.lock().map_err(|_| Error::Stopped)?;
        if installed.is_some() || self.stopped.load(Ordering::Acquire) {
            let _ = socket.shutdown(Shutdown::Both);
            return Err(Error::Stopped);
        }
        *installed = Some(socket);
        Ok(())
    }
}

/// Terminal control of one original microphone, independent from input revoke.
/// An escaped old control never targets a replacement with equal wire IDs.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn stop(&self) {
        self.0.stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.0.check().is_err()
    }
}
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMicrophoneProcessControl")
            .field("stopped", &self.0.stopped.load(Ordering::Acquire))
            .finish()
    }
}
#[derive(Clone)]
pub struct Retirement(Arc<Shared>);
impl Retirement {
    pub fn stop(&self) {
        self.0.stop();
    }
    pub fn receipts(&self) -> ExitReceipts {
        ExitReceipts {
            device: exit_status(self.0.device_exit.load(Ordering::Acquire)),
            codec: exit_status(self.0.codec_exit.load(Ordering::Acquire)),
        }
    }
    pub fn is_complete(&self) -> bool {
        self.0.complete.load(Ordering::Acquire)
    }
    /// Immutable original IDs, available after both spawns. Interpret them
    /// alongside `receipts`: a number is never proof of liveness or reaping and
    /// may be reused by the OS after exit. All production control retains the
    /// actual original Child/socket ownership instead of looking up these IDs.
    pub fn child_ids(&self) -> Option<ChildIds> {
        let device = self.0.device_pid.load(Ordering::Acquire);
        let codec = self.0.codec_pid.load(Ordering::Acquire);
        (device != 0 && codec != 0).then_some(ChildIds { device, codec })
    }
}
impl std::fmt::Debug for Retirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMicrophoneRetirement")
            .field("receipts", &self.receipts())
            .field("complete", &self.is_complete())
            .finish()
    }
}

/// One explicitly approved, bounded host microphone owner. All public methods
/// do pure policy/framing work; foreign calls and child waits stay on its one
/// supervisor. Drop fences immediately; Retirement separately reports both exits.
pub struct Process {
    shared: Arc<Shared>,
}
impl std::fmt::Debug for Process {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMicrophoneProcess")
            .field("state", &self.shared.lock().state)
            .finish_non_exhaustive()
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.shared.stop();
    }
}
impl Process {
    /// `now` samples the same clock as the actual `InputSession` and datagram
    /// receipt metadata. This projection, separate local approval, negotiated
    /// profile and completed route are all validated before starting a child.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        controller: HostController,
        channel: Channel,
        selection: &Selection,
        approved: bool,
        configuration: &[u8],
        launch: Launch,
        now: impl FnOnce() -> HostInstant,
    ) -> Result<Self, Error> {
        validate_scope(&controller, channel, selection, approved)?;
        let offer = audio::decode_configuration(configuration, channel.id)
            .map_err(|_| Error::Configuration)?;
        let options = Configuration {
            server: launch.server.clone(),
            offer,
            packet_maximum: channel.packet_maximum,
        };
        options.validate()?;
        let shared = Arc::new(Shared {
            channel,
            options,
            controller,
            clock: Clock::bind(now)?,
            stopped: AtomicBool::new(false),
            revoke: Mutex::new(None),
            mail: Mutex::new(Mail {
                state: State::Starting,
                job: None,
                packet_busy: false,
                packet_reply: None,
                snapshot: None,
                positive: None,
                error: None,
            }),
            wake: Condvar::new(),
            device_exit: AtomicU8::new(0),
            codec_exit: AtomicU8::new(0),
            device_pid: AtomicU32::new(0),
            codec_pid: AtomicU32::new(0),
            complete: AtomicBool::new(false),
        });
        shared.check()?;
        OWNERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_OWNERS).then_some(count + 1)
            })
            .map_err(|_| Error::Limit)?;
        let worker = shared.clone();
        if std::thread::Builder::new()
            .name("fr-host-microphone".into())
            .spawn(move || supervise(&worker, &launch))
            .is_err()
        {
            OWNERS.fetch_sub(1, Ordering::AcqRel);
            shared.complete.store(true, Ordering::Release);
            shared.fail(Error::Spawn);
            return Err(Error::Spawn);
        }
        Ok(Self { shared })
    }
    pub fn control(&self) -> Control {
        Control(self.shared.clone())
    }
    pub fn retirement(&self) -> Retirement {
        Retirement(self.shared.clone())
    }
    pub fn state(&self) -> State {
        let _ = self.shared.check();
        self.shared.lock().state
    }
    pub fn error(&self) -> Option<Error> {
        self.shared.lock().error
    }
    pub fn snapshot(&self) -> Option<Snapshot> {
        self.shared.lock().snapshot
    }
    pub fn poll_ready(&self) -> Result<Option<Snapshot>, Error> {
        self.shared.check()?;
        let mail = self.shared.lock();
        Ok((mail.state == State::Ready)
            .then_some(mail.snapshot)
            .flatten())
    }
    /// Success from `send` must mean admission of these exact bytes to the
    /// original authenticated route. This method queues the second Commit;
    /// `state() == Running` confirms its matching private reply. No packet can
    /// be queued during readiness or unknown/failed admission, and no retry is
    /// allowed after the callback has been entered.
    pub fn acknowledge(&mut self, send: impl FnOnce(&[u8]) -> Result<(), ()>) -> Result<(), Error> {
        self.shared.check()?;
        let record = {
            let mut mail = self.shared.lock();
            if mail.state != State::Ready {
                return Err(Error::NotReady);
            }
            let record = mail.positive.take().ok_or(Error::Protocol)?;
            mail.state = State::Committing;
            record
        };
        if catch_unwind(AssertUnwindSafe(|| send(&record)))
            .unwrap_or(Err(()))
            .is_err()
        {
            self.shared.fail(Error::AcknowledgementUnknown);
            return Err(Error::AcknowledgementUnknown);
        }
        self.shared.check()?;
        let mut mail = self.shared.lock();
        if mail.state != State::Committing || mail.job.is_some() {
            drop(mail);
            self.shared.fail(Error::AcknowledgementUnknown);
            return Err(Error::AcknowledgementUnknown);
        }
        mail.job = Some(Job::Commit);
        drop(mail);
        self.shared.wake.notify_all();
        Ok(())
    }
    /// Use the ORIGINAL authenticated datagram receipt and its earlier expiry
    /// in the same host clock as `start`. These are sampled before every inbox;
    /// a worker dequeue must never supply current time as a new receipt.
    pub fn receive_record_until(
        &mut self,
        bytes: &[u8],
        arrived: HostInstant,
        expires: HostInstant,
    ) -> Result<Admission, Error> {
        if let Ok(stop) = audio::decode_stop(bytes, self.shared.channel.id)
            && stop.direction == AudioDirection::Uplink
            && stop.generation == self.shared.options.offer.generation
        {
            self.shared.stop();
            return Ok(Admission::Stopped);
        }
        self.shared.check()?;
        let result = self.admit_packet(bytes, arrived, expires);
        if let Err(error) = result
            && !matches!(error, Error::Busy | Error::NotReady)
        {
            self.shared.fail(error);
        }
        result
    }
    fn admit_packet(
        &mut self,
        bytes: &[u8],
        arrived: HostInstant,
        expires: HostInstant,
    ) -> Result<Admission, Error> {
        if bytes.len() > self.shared.channel.packet_maximum {
            return Err(Error::Configuration);
        }
        if audio::decode_stop(bytes, self.shared.channel.id).is_ok() {
            return Ok(Admission::Ignored);
        }
        let packet =
            audio::decode_packet(bytes, self.shared.channel.id).map_err(|_| Error::Protocol)?;
        if packet.direction != AudioDirection::Uplink
            || packet.generation != self.shared.options.offer.generation
        {
            return Ok(Admission::Ignored);
        }
        let config = self.shared.options.validate()?;
        if packet.payload.len() > self.shared.options.offer.max_packet_bytes as usize
            || u32::from(packet.duration_samples) != config.expected_samples_per_frame()
        {
            return Err(Error::Configuration);
        }
        let Some((arrived_ns, expires_ns)) = original_receipt(&self.shared, arrived, expires)?
        else {
            return Ok(Admission::Ignored);
        };
        let mut body = Vec::new();
        body.try_reserve_exact(bytes.len())
            .map_err(|_| Error::Limit)?;
        body.extend_from_slice(bytes);
        let packet = Packet {
            body,
            arrived_ns,
            expires_ns,
        };
        let mut mail = self.shared.lock();
        if mail.state != State::Running {
            return Err(Error::NotReady);
        }
        if mail.packet_busy || mail.job.is_some() {
            return Err(Error::Busy);
        }
        mail.packet_busy = true;
        mail.job = Some(Job::Packet(packet));
        drop(mail);
        self.shared.wake.notify_all();
        Ok(Admission::Queued)
    }
    /// One result must be collected before another packet can enter this owner.
    /// Admitted is bounded native/jitter custody, never proof an app heard it.
    pub fn poll_packet(&mut self) -> Result<Option<PacketOutcome>, Error> {
        self.shared.check()?;
        let mut mail = self.shared.lock();
        let reply = mail.packet_reply.take();
        if reply.is_some() {
            mail.packet_busy = false;
        }
        Ok(reply)
    }
    pub fn stop(&self) {
        self.shared.stop();
    }
}

fn validate_scope(
    controller: &HostController,
    channel: Channel,
    selection: &Selection,
    approved: bool,
) -> Result<(), Error> {
    if !approved {
        return Err(Error::Permission);
    }
    if !fr_wire::audio_up::uplink_selected(selection) {
        return Err(Error::NotSelected);
    }
    let scope = controller.scope();
    if scope.session != channel.parent.remote_session
        || scope.lease.as_raw() == 0
        || channel.parent.id == 0
        || channel.parent.host_boot.as_raw() == 0
        || channel.parent.os_session.as_raw() == 0
        || scope.session.as_raw() == 0
        || channel.id == 0
        || channel.id == channel.parent.id
        || channel.packet_maximum <= audio::AUDIO_PACKET_OVERHEAD
        || channel.packet_maximum > ipc::MAX_BODY_BYTES
    {
        return Err(Error::Binding);
    }
    Ok(())
}
fn monotonic_ns() -> Result<u64, Error> {
    let stamp = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(stamp.tv_sec)
        .ok()
        .and_then(|sec| sec.checked_mul(1_000_000_000))
        .and_then(|base| {
            u64::try_from(stamp.tv_nsec)
                .ok()
                .and_then(|ns| base.checked_add(ns))
        })
        .ok_or(Error::Clock)
}
fn original_receipt(
    shared: &Shared,
    arrived: HostInstant,
    expires: HostInstant,
) -> Result<Option<(u64, u64)>, Error> {
    let raw = monotonic_ns()?;
    let now = shared.clock.now()?.as_micros();
    let arrived = arrived.as_micros();
    if arrived > now || expires.as_micros() <= arrived {
        return Err(Error::Clock);
    }
    let expires = expires.as_micros().min(
        arrived
            .checked_add(ipc::MAX_RECORD_NS / 1000)
            .ok_or(Error::Clock)?,
    );
    if now >= expires {
        return Ok(None);
    }
    let arrived_ns = raw
        .checked_sub((now - arrived).checked_mul(1000).ok_or(Error::Clock)?)
        .ok_or(Error::Clock)?;
    let expires_ns = raw
        .checked_add((expires - now).checked_mul(1000).ok_or(Error::Clock)?)
        .ok_or(Error::Clock)?;
    Ok(Some((arrived_ns, expires_ns)))
}

struct Children {
    device: Option<Child>,
    codec: Option<Child>,
    shared: Arc<Shared>,
}
impl Children {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            device: None,
            codec: None,
            shared,
        }
    }
    fn alive(&mut self) -> Result<(), Error> {
        for child in [&mut self.device, &mut self.codec] {
            if let Some(child) = child.as_mut()
                && child.try_wait().map_err(|_| Error::Protocol)?.is_some()
            {
                return Err(Error::Stopped);
            }
        }
        Ok(())
    }
    fn retire(&mut self) {
        // Signal BOTH children before either wait: a native destructor or an
        // uninterruptible device must never postpone killing its codec sibling.
        if let Some(child) = self.device.as_mut() {
            let _ = child.kill();
        }
        if let Some(child) = self.codec.as_mut() {
            let _ = child.kill();
        }
        for (child, receipt) in [
            (&mut self.device, &self.shared.device_exit),
            (&mut self.codec, &self.shared.codec_exit),
        ] {
            if let Some(mut child) = child.take() {
                receipt.store(if child.wait().is_ok() { 2 } else { 3 }, Ordering::Release);
            }
        }
        let settled = [&self.shared.device_exit, &self.shared.codec_exit]
            .into_iter()
            .all(|receipt| matches!(receipt.load(Ordering::Acquire), 0 | 2));
        if settled && !self.shared.complete.swap(true, Ordering::AcqRel) {
            OWNERS.fetch_sub(1, Ordering::AcqRel);
        }
        // A failed/uncompleted wait retains its global owner slot permanently;
        // a requested stop or channel closure is never an exit receipt.
    }
}
impl Drop for Children {
    fn drop(&mut self) {
        self.retire();
    }
}

fn spawn(
    shared: &Arc<Shared>,
    launch: &Launch,
    children: &mut Children,
) -> Result<UnixStream, Error> {
    shared.check()?;
    let (command, device_command) = UnixStream::pair().map_err(|_| Error::Spawn)?;
    let (revoke, device_revoke) = UnixStream::pair().map_err(|_| Error::Spawn)?;
    let (codec_parent, device_codec) = UnixStream::pair().map_err(|_| Error::Spawn)?;
    command.set_nonblocking(true).map_err(|_| Error::Spawn)?;
    revoke.set_nonblocking(true).map_err(|_| Error::Spawn)?;
    shared.bind_revoke(Arc::new(revoke))?;
    let codec_output = codec_parent.try_clone().map_err(|_| Error::Spawn)?;
    let mut codec = Command::new(&launch.codec);
    codec
        .env_clear()
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::from(OwnedFd::from(codec_parent)))
        .stdout(Stdio::from(OwnedFd::from(codec_output)))
        .stderr(Stdio::inherit())
        .process_group(0);
    let child = codec.spawn().map_err(|_| Error::Spawn)?;
    shared.codec_pid.store(child.id(), Ordering::Release);
    children.codec = Some(child);
    shared.codec_exit.store(1, Ordering::Release);
    drop(codec);
    shared.check()?;
    let mut device = Command::new(&launch.device);
    device
        .env_clear()
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::from(OwnedFd::from(device_command)))
        .stdout(Stdio::from(OwnedFd::from(device_codec)))
        .stderr(Stdio::from(OwnedFd::from(device_revoke)))
        .process_group(0);
    let child = device.spawn().map_err(|_| Error::Spawn)?;
    shared.device_pid.store(child.id(), Ordering::Release);
    children.device = Some(child);
    shared.device_exit.store(1, Ordering::Release);
    drop(device);
    shared.check()?;
    Ok(command)
}

fn header(
    shared: &Shared,
    kind: Kind,
    serial: u64,
    bytes: usize,
    packet: Option<&Packet>,
    predecessor: u64,
) -> Result<Header, Error> {
    let (now, authorization_ns) = shared.check()?;
    let ceiling = if kind == Kind::Configure {
        ipc::SETUP_NS
    } else {
        ipc::MAX_RECORD_NS
    };
    let arrived_ns = packet.map_or(now, |packet| packet.arrived_ns);
    let expires_ns = packet
        .map_or_else(
            || now.checked_add(ceiling).ok_or(Error::Clock),
            |packet| Ok(packet.expires_ns),
        )?
        .min(authorization_ns)
        .min(predecessor);
    let header = Header {
        kind,
        status: Status::Ok,
        serial,
        generation: shared.options.offer.generation.as_raw(),
        authorization_ns,
        arrived_ns,
        expires_ns,
        binding: shared.channel.id,
        bytes: u32::try_from(bytes).map_err(|_| Error::Limit)?,
    };
    header.validate(false)?;
    header.live(now)?;
    Ok(header)
}
fn exchange(
    shared: &Shared,
    command: &mut UnixStream,
    children: &mut Children,
    request: &Record,
) -> Result<Record, Error> {
    let mut outgoing = Outgoing::new(request, false)?;
    let mut incoming = Incoming::default();
    let mut sent = false;
    loop {
        shared.check()?;
        children.alive()?;
        request.header.live(monotonic_ns()?)?;
        if !sent {
            sent = outgoing.poll(command)?;
        }
        if sent && let Some(reply) = incoming.poll(command, true)? {
            if !reply.header.matches(request.header) {
                return Err(Error::Protocol);
            }
            if reply.header.status != Status::Ok {
                return Err(Error::Native(reply.header.status));
            }
            request.header.live(monotonic_ns()?)?;
            shared.check()?;
            return Ok(reply);
        }
        std::thread::sleep(POLL);
    }
}
fn idle_channel(command: &mut UnixStream) -> Result<(), Error> {
    let mut byte = [0];
    match command.read(&mut byte) {
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        _ => Err(Error::Protocol),
    }
}
fn ready(shared: &Shared, reply: &Record) -> Result<(), Error> {
    if reply.body.len() != ipc::SNAPSHOT_BYTES + audio::AUDIO_CONFIGURED_RECORD_BYTES {
        return Err(Error::Protocol);
    }
    let snapshot = Snapshot::decode(&reply.body[..ipc::SNAPSHOT_BYTES])?;
    if snapshot.node_id == 0
        || snapshot.node_id == u32::MAX
        || snapshot.failure_code != 0
        || snapshot.queued_frames != 0
        || snapshot.queued_packets != 0
        || snapshot.admitted_packet
    {
        return Err(Error::Protocol);
    }
    let positive: [u8; audio::AUDIO_CONFIGURED_RECORD_BYTES] = reply.body[ipc::SNAPSHOT_BYTES..]
        .try_into()
        .map_err(|_| Error::Protocol)?;
    let actual =
        audio::decode_configured(&positive, shared.channel.id).map_err(|_| Error::Protocol)?;
    let expected = shared.options.validate()?;
    if !actual.accepted
        || actual.direction != expected.direction()
        || actual.generation != expected.generation()
        || actual.actual_sample_rate != expected.sample_rate()
        || actual.actual_channels != expected.channels()
        || actual.actual_frame_duration_ms != expected.frame_duration_ms()
    {
        return Err(Error::Protocol);
    }
    let mut mail = shared.lock();
    if shared.stopped.load(Ordering::Acquire) || mail.state != State::Starting {
        return Err(Error::Stopped);
    }
    mail.snapshot = Some(snapshot);
    mail.positive = Some(positive);
    mail.state = State::Ready;
    Ok(())
}
fn snapshot_reply(shared: &Shared, reply: &Record, kind: Kind) -> Result<(), Error> {
    let snapshot = Snapshot::decode(&reply.body)?;
    let mut mail = shared.lock();
    if shared.stopped.load(Ordering::Acquire) {
        return Err(Error::Stopped);
    }
    if (kind != Kind::Packet && snapshot.admitted_packet)
        || snapshot.failure_code != 0
        || mail.snapshot.is_none_or(|old| {
            old.node_id != snapshot.node_id
                || snapshot.submitted_frames < old.submitted_frames
                || snapshot.silent_frames < old.silent_frames
                || snapshot.expired_frames < old.expired_frames
        })
    {
        return Err(Error::Protocol);
    }
    mail.snapshot = Some(snapshot);
    match kind {
        Kind::Commit => {
            if mail.state != State::Committing {
                return Err(Error::Protocol);
            }
            mail.state = State::Running;
        }
        Kind::Packet => {
            mail.packet_reply = Some(if snapshot.admitted_packet {
                PacketOutcome::Admitted
            } else {
                PacketOutcome::Ignored
            });
        }
        Kind::Health if !snapshot.admitted_packet => {}
        _ => return Err(Error::Protocol),
    }
    Ok(())
}
fn run(shared: &Arc<Shared>, launch: &Launch, children: &mut Children) -> Result<(), Error> {
    let mut command = spawn(shared, launch, children)?;
    let body = shared.options.encode(shared.channel.id)?;
    let request = Record {
        header: header(shared, Kind::Configure, 1, body.len(), None, u64::MAX)?,
        body,
    };
    ready(shared, &exchange(shared, &mut command, children, &request)?)?;
    let mut installed_cap = request.header.authorization_ns;
    let mut serial = 1_u64;
    let mut health = Instant::now().checked_add(HEALTH).ok_or(Error::Clock)?;
    loop {
        shared.check()?;
        children.alive()?;
        idle_channel(&mut command)?;
        let job = shared.lock().job.take();
        if job.is_none() && Instant::now() < health {
            let mail = shared.lock();
            let _ = shared
                .wake
                .wait_timeout(mail, POLL)
                .map_err(|_| Error::Stopped)?;
            continue;
        }
        let kind = match &job {
            Some(Job::Commit) => Kind::Commit,
            Some(Job::Packet(_)) => Kind::Packet,
            None => Kind::Health,
        };
        if let Some(Job::Packet(packet)) = &job
            && monotonic_ns()? >= packet.expires_ns
        {
            shared.lock().packet_reply = Some(PacketOutcome::Ignored);
            continue;
        }
        serial = serial.checked_add(1).ok_or(Error::Protocol)?;
        let request = match job {
            Some(Job::Packet(mut packet)) => Record {
                header: header(
                    shared,
                    kind,
                    serial,
                    packet.body.len(),
                    Some(&packet),
                    installed_cap,
                )?,
                body: std::mem::take(&mut packet.body),
            },
            _ => Record {
                header: header(shared, kind, serial, 0, None, installed_cap)?,
                body: Vec::new(),
            },
        };
        let reply = exchange(shared, &mut command, children, &request)?;
        snapshot_reply(shared, &reply, kind)?;
        installed_cap = reply.header.authorization_ns;
        health = Instant::now().checked_add(HEALTH).ok_or(Error::Clock)?;
    }
}
fn supervise(shared: &Arc<Shared>, launch: &Launch) {
    let mut children = Children::new(shared.clone());
    let result = catch_unwind(AssertUnwindSafe(|| run(shared, launch, &mut children)))
        .unwrap_or(Err(Error::Protocol));
    if let Err(error) = result {
        shared.fail(error);
    } else {
        shared.stop();
    }
    children.retire();
}
