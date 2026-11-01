#![forbid(unsafe_code)]
//! Explicit microphone capture in one supervised native child.
//!
//! The caller supplies the original session's durable, nonblocking permission
//! monitor and absolute authorization deadline. The supervisor checks both even
//! when the caller stops polling. A terminal private channel also withdraws the
//! child's permission; no request, replacement or retained batch can revive it.
//! Process separation contains native hangs/crashes, not a same-user sandbox.
//! There is no network attachment, default source, automatic enable or runtime.
pub mod child;
mod protocol;
mod supervisor;
pub mod transmit;

use crate::pulse::{Error as PulseError, microphone::MAX_CAPTURE_CUSTODY};
use fr_core::audio::{AudioDirection, AudioStreamConfig, MAX_OPUS_PAYLOAD_BYTES};
use fr_media::{audio::AudioAccessUnit, worker::audio::CaptureCounters};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const SETUP: Duration = Duration::from_secs(2);
const RENEWAL_EXCHANGE: Duration = Duration::from_millis(100);
const MONITOR_TURN: Duration = Duration::from_millis(10);
const MAX_WORKERS: usize = 4;
static WORKERS: AtomicUsize = AtomicUsize::new(0);

/// Content-free, terminal source/process failures. Backpressure alone is not
/// terminal: it admits no second request while the original slot is occupied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Error {
    Configuration = 1,
    Selection,
    Unavailable,
    Denied,
    Expired,
    Clock,
    Backpressure,
    Protocol,
    Spawn,
    Native,
    Stopped,
    CaptureDiscontinuity,
    Allocation,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "microphone-process: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<PulseError> for Error {
    fn from(error: PulseError) -> Self {
        match error {
            PulseError::Configuration => Self::Configuration,
            PulseError::Selection => Self::Selection,
            PulseError::Unavailable | PulseError::DeviceChanged | PulseError::Suspended => {
                Self::Unavailable
            }
            PulseError::Denied => Self::Denied,
            PulseError::Expired => Self::Expired,
            PulseError::Clock => Self::Clock,
            PulseError::Allocation => Self::Allocation,
            PulseError::Backpressure | PulseError::Pending => Self::Backpressure,
            PulseError::Closed => Self::Stopped,
            PulseError::CaptureDiscontinuity => Self::CaptureDiscontinuity,
            PulseError::Metadata | PulseError::BufferLimit => Self::Protocol,
            PulseError::Native | PulseError::SubmissionUnknown => Self::Native,
        }
    }
}

/// Locally selected, explicit source and one immutable uplink generation.
/// Debug never reveals the server path, device name or captured contents.
#[derive(Clone)]
pub struct CaptureOptions {
    pub server: PathBuf,
    pub source: String,
    pub configuration: AudioStreamConfig,
    pub bitrate: u32,
    pub max_packet_bytes: u16,
}
impl std::fmt::Debug for CaptureOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneCaptureOptions")
            .field("configuration", &self.configuration)
            .field("bitrate", &self.bitrate)
            .field("max_packet_bytes", &self.max_packet_bytes)
            .finish_non_exhaustive()
    }
}
impl CaptureOptions {
    pub fn validate(&self) -> Result<(), Error> {
        let c = self.configuration;
        if c.direction() != AudioDirection::Uplink
            || c.generation().as_raw() == 0
            || !matches!(c.frame_duration_ms(), 10 | 20)
            || !(6_000..=192_000).contains(&self.bitrate)
            || self.max_packet_bytes == 0
            || usize::from(self.max_packet_bytes) > MAX_OPUS_PAYLOAD_BYTES
            || u64::from(self.bitrate) * u64::from(c.frame_duration_ms())
                > u64::from(self.max_packet_bytes) * 8_000
        {
            return Err(Error::Configuration);
        }
        let server = self.server.to_str().ok_or(Error::Selection)?;
        if !self.server.is_absolute()
            || self
                .server
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || !(2..=100).contains(&server.len())
            || server.bytes().any(|b| b <= b' ' || b == 127 || b == b':')
            || self.source.is_empty()
            || self.source.len() > 255
            || !self
                .source
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        {
            return Err(Error::Selection);
        }
        Ok(())
    }
}

struct Slot;
impl Slot {
    fn take() -> Result<Self, Error> {
        WORKERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_WORKERS).then_some(count + 1)
            })
            .map(|_| Self)
            .map_err(|_| Error::Backpressure)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Packets {
    units: Vec<AudioAccessUnit>,
    counters: CaptureCounters,
    expires: Instant,
}
#[derive(Clone, Copy)]
struct Authorization {
    until: Instant,
    ns: u64,
}
enum Request {
    Pull(Instant),
    Renew {
        authorization: Authorization,
        expires: Instant,
    },
}
enum Reply {
    Ready,
    Renewed,
    Packets(Packets),
}
#[derive(Default)]
struct Mailbox {
    request: Option<Request>,
    reply: Option<Reply>,
    error: Option<Error>,
    revoke: Option<std::os::unix::net::UnixStream>,
}
struct Shared {
    mail: Mutex<Mailbox>,
    wake: Condvar,
    stopped: AtomicBool,
    retired: AtomicBool,
    pid: AtomicU32,
    authorization: Mutex<Authorization>,
    monitor: Arc<dyn Fn() -> bool + Send + Sync>,
}
impl Shared {
    fn stop(&self, error: Error) {
        // Fence retained results before acquiring any mailbox or doing I/O.
        self.stopped.store(true, Ordering::Release);
        if let Ok(mut mail) = self.mail.lock() {
            mail.error.get_or_insert(error);
            mail.request = None;
            mail.reply = None;
            // EOF is the child's independent final permission fence. This is
            // a private channel; it never carries a new approval or a renewal.
            mail.revoke = None;
        }
        self.wake.notify_one();
    }
    fn error(&self) -> Error {
        self.mail
            .lock()
            .ok()
            .and_then(|mail| mail.error)
            .unwrap_or(Error::Stopped)
    }
    fn authorization(&self) -> Result<Authorization, Error> {
        self.authorization
            .lock()
            .map(|authorization| *authorization)
            .map_err(|_| Error::Stopped)
    }
    fn install_authorization(
        &self,
        old_ns: u64,
        next: Authorization,
        exchange_until: Instant,
    ) -> Result<(), Error> {
        self.check()?;
        {
            let mut current = self.authorization.lock().map_err(|_| Error::Stopped)?;
            if current.ns != old_ns || self.stopped.load(Ordering::Acquire) {
                return Err(Error::Stopped);
            }
            // The child acknowledgement, not request enqueue, commits this
            // future cap. A delayed acknowledgement cannot revive the old one.
            let now = Instant::now();
            if now >= current.until || now >= next.until || now >= exchange_until {
                return Err(Error::Expired);
            }
            *current = next;
        }
        self.check()
    }
    fn check(&self) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(self.error());
        }
        // Application callbacks never execute while holding a mailbox lock.
        let before_callback = self.authorization()?;
        let error = if Instant::now() >= before_callback.until {
            Some(Error::Expired)
        } else if !catch_unwind(AssertUnwindSafe(|| (self.monitor)())).unwrap_or(false) {
            Some(Error::Denied)
        } else if self.stopped.load(Ordering::Acquire) {
            Some(self.error())
        } else if Instant::now() >= self.authorization()?.until {
            Some(Error::Expired)
        } else {
            None
        };
        if let Some(error) = error {
            self.stop(error);
            Err(error)
        } else {
            Ok(())
        }
    }
    fn reply(&self, reply: Reply) -> Result<(), Error> {
        self.check()?;
        let mut mail = self.mail.lock().map_err(|_| Error::Stopped)?;
        if self.stopped.load(Ordering::Acquire) || mail.reply.is_some() {
            return Err(Error::Stopped);
        }
        mail.reply = Some(reply);
        Ok(())
    }
}

/// Terminal signal for the original child and every batch it produced.
/// Cloning it cannot keep a dropped capture owner alive or target a replacement.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl Control {
    pub fn stop(&self) {
        self.0.stop(Error::Stopped);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
            || !self
                .0
                .authorization()
                .is_ok_and(|authorization| Instant::now() < authorization.until)
    }
}
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneProcessControl")
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

/// Requested stop and actual child destruction are deliberately separate.
#[derive(Clone)]
pub struct Retirement(Arc<Shared>);
impl Retirement {
    pub fn stop(&self) {
        self.0.stop(Error::Stopped);
    }
    pub fn is_complete(&self) -> bool {
        self.0.retired.load(Ordering::Acquire)
    }
    pub fn process_id(&self) -> Option<u32> {
        let pid = self.0.pid.load(Ordering::Acquire);
        (pid != 0).then_some(pid)
    }
}
impl std::fmt::Debug for Retirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneProcessRetirement")
            .field("complete", &self.is_complete())
            .finish_non_exhaustive()
    }
}

/// One bounded capture result. IPC time and earlier partial PCM retain their
/// original deadline. Consuming this does not grant transport admission: the
/// sender must preserve `expires` and check permission at its own boundary.
pub struct Batch {
    packets: Packets,
    control: Control,
}
impl Batch {
    pub fn packet_count(&self) -> usize {
        self.packets.units.len()
    }
    pub const fn expires(&self) -> Instant {
        self.packets.expires
    }
    pub const fn counters(&self) -> CaptureCounters {
        self.packets.counters
    }
    pub fn consume(
        self,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Vec<AudioAccessUnit>, Error> {
        gate(&self.control.0, &mut permitted)?;
        if Instant::now() >= self.expires() {
            self.control.0.stop(Error::Expired);
            return Err(Error::Expired);
        }
        Ok(self.packets.units)
    }
}
impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneProcessBatch")
            .field("packets", &self.packet_count())
            .field("counters", &self.counters())
            .finish_non_exhaustive()
    }
}

#[derive(PartialEq, Eq)]
enum Phase {
    Starting,
    Idle,
    Pulling,
    Renewing,
    Stopped,
}
/// Nonblocking caller-side owner. Native setup, capture, encode, IPC and reap
/// all belong to the one bounded supervisor/child pair, never this caller.
pub struct MicrophoneCapture {
    shared: Arc<Shared>,
    config: AudioStreamConfig,
    phase: Phase,
    until: Option<Instant>,
    next: Option<(u64, u64)>,
}
impl std::fmt::Debug for MicrophoneCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MicrophoneCapture([private original process owner])")
    }
}
impl MicrophoneCapture {
    /// Start only after explicit per-session talk enable, actual OS permission
    /// and the original controlling session's admitted microphone capability.
    /// `monitor` must independently observe their revocation and be nonblocking;
    /// it is retained and evaluated even if the caller stops servicing this API.
    /// `authorization_until` is the initial original-owner proof deadline.
    /// Later fresh proof requires an explicit acknowledged `renew_authority`
    /// exchange; neither polling nor new capture requests extend it.
    pub fn start(
        image: &Path,
        options: CaptureOptions,
        authorization_until: Instant,
        monitor: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        options.validate()?;
        let authorization_ns = crate::clock::stamp_ns(authorization_until).ok_or(Error::Clock)?;
        let shared = Arc::new(Shared {
            mail: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            retired: AtomicBool::new(true),
            pid: AtomicU32::new(0),
            authorization: Mutex::new(Authorization {
                until: authorization_until,
                ns: authorization_ns,
            }),
            monitor: Arc::new(monitor),
        });
        shared.check()?;
        if !image.is_absolute() || !image.is_file() {
            return Err(Error::Unavailable);
        }
        let slot = Slot::take()?;
        let until = Instant::now()
            .checked_add(SETUP)
            .ok_or(Error::Clock)?
            .min(authorization_until);
        let config = options.configuration;
        shared.check()?;
        shared.retired.store(false, Ordering::Release);
        let (image, worker) = (image.to_owned(), shared.clone());
        if std::thread::Builder::new()
            .name("fr-microphone-owner".into())
            .spawn(move || supervisor::run(image, &options, until, &worker, slot))
            .is_err()
        {
            shared.retired.store(true, Ordering::Release);
            shared.stop(Error::Spawn);
            return Err(Error::Spawn);
        }
        Ok(Self {
            shared,
            config,
            phase: Phase::Starting,
            until: Some(until),
            next: None,
        })
    }
    pub fn control(&self) -> Control {
        Control(self.shared.clone())
    }
    pub fn retirement(&self) -> Retirement {
        Retirement(self.shared.clone())
    }
    pub const fn configuration(&self) -> AudioStreamConfig {
        self.config
    }
    fn fail(&mut self, error: Error) -> Error {
        self.shared.stop(error);
        self.phase = Phase::Stopped;
        error
    }
    fn collect(&mut self, permitted: &mut impl FnMut() -> bool) -> Result<Option<Reply>, Error> {
        gate(&self.shared, permitted)?;
        if self.until.is_some_and(|until| Instant::now() >= until) {
            return Err(self.fail(Error::Expired));
        }
        let reply = self
            .shared
            .mail
            .lock()
            .map_err(|_| Error::Stopped)?
            .reply
            .take();
        gate(&self.shared, permitted)?;
        if self.until.is_some_and(|until| Instant::now() >= until) {
            return Err(self.fail(Error::Expired));
        }
        Ok(reply)
    }
    pub fn poll_ready(&mut self, mut permitted: impl FnMut() -> bool) -> Result<bool, Error> {
        gate(&self.shared, &mut permitted)?;
        if matches!(self.phase, Phase::Idle | Phase::Pulling | Phase::Renewing) {
            return Ok(true);
        }
        if self.phase != Phase::Starting {
            return Err(Error::Stopped);
        }
        match self.collect(&mut permitted)? {
            Some(Reply::Ready) => {
                self.phase = Phase::Idle;
                self.until = None;
                Ok(true)
            }
            Some(_) => Err(self.fail(Error::Protocol)),
            None => Ok(false),
        }
    }
    /// Admit one pull, retaining the supplied original deadline through IPC.
    /// No capture request or retry can renew partial PCM retained in the child.
    pub fn request(
        &mut self,
        expires: Instant,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        gate(&self.shared, &mut permitted)?;
        if self.phase != Phase::Idle {
            return Err(Error::Backpressure);
        }
        let now = Instant::now();
        if now >= expires {
            return Err(self.fail(Error::Expired));
        }
        if expires > now.checked_add(MAX_CAPTURE_CUSTODY).ok_or(Error::Clock)? {
            return Err(self.fail(Error::Configuration));
        }
        let expires = expires.min(self.shared.authorization()?.until);
        {
            let mut mail = self.shared.mail.lock().map_err(|_| Error::Stopped)?;
            if self.shared.stopped.load(Ordering::Acquire) {
                return Err(mail.error.unwrap_or(Error::Stopped));
            }
            if mail.request.is_some() || mail.reply.is_some() {
                return Err(Error::Backpressure);
            }
            mail.request = Some(Request::Pull(expires));
        }
        self.phase = Phase::Pulling;
        self.until = Some(expires);
        self.shared.wake.notify_one();
        Ok(())
    }
    /// Stage one renewal from the original permission monitor. This only
    /// queues a bounded control exchange; the installed process cap remains
    /// unchanged until the child acknowledges before the OLD cap expires.
    /// No pull may overlap the exchange. Existing partial PCM keeps its own
    /// capture deadline and can never borrow the proposed authorization.
    pub fn renew_authority(
        &mut self,
        authorization_until: Instant,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        gate(&self.shared, &mut permitted)?;
        if self.phase != Phase::Idle {
            return Err(Error::Backpressure);
        }
        let current = self.shared.authorization()?;
        let now = Instant::now();
        if now >= authorization_until {
            return Err(self.fail(Error::Expired));
        }
        let ns = crate::clock::stamp_ns(authorization_until).ok_or(Error::Clock)?;
        if ns == current.ns {
            return Err(Error::Configuration);
        }
        let expires = now
            .checked_add(RENEWAL_EXCHANGE)
            .ok_or(Error::Clock)?
            .min(current.until)
            .min(authorization_until);
        let authorization = Authorization {
            until: authorization_until,
            ns,
        };
        {
            let mut mail = self.shared.mail.lock().map_err(|_| Error::Stopped)?;
            if self.shared.stopped.load(Ordering::Acquire) {
                return Err(mail.error.unwrap_or(Error::Stopped));
            }
            if mail.request.is_some() || mail.reply.is_some() {
                return Err(Error::Backpressure);
            }
            mail.request = Some(Request::Renew {
                authorization,
                expires,
            });
        }
        self.phase = Phase::Renewing;
        self.until = Some(expires);
        self.shared.wake.notify_one();
        Ok(())
    }
    /// Collect the exact renewal acknowledgement. Unknown, late or mismatched
    /// completion is terminal; it is never transparently replayed.
    pub fn poll_renewed(&mut self, mut permitted: impl FnMut() -> bool) -> Result<bool, Error> {
        gate(&self.shared, &mut permitted)?;
        if self.phase != Phase::Renewing {
            return Err(Error::Backpressure);
        }
        match self.collect(&mut permitted)? {
            None => Ok(false),
            Some(Reply::Renewed) => {
                self.phase = Phase::Idle;
                self.until = None;
                Ok(true)
            }
            Some(_) => Err(self.fail(Error::Protocol)),
        }
    }
    /// Last positively acknowledged native authorization. A staged renewal
    /// never changes this value. This is metadata, not a permission grant.
    pub fn authorization_deadline(&self) -> Result<Instant, Error> {
        self.shared
            .authorization()
            .map(|authorization| authorization.until)
    }
    pub fn poll_batch(
        &mut self,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Option<Batch>, Error> {
        gate(&self.shared, &mut permitted)?;
        if self.phase != Phase::Pulling {
            return Err(Error::Backpressure);
        }
        match self.collect(&mut permitted)? {
            None => Ok(None),
            Some(Reply::Ready | Reply::Renewed) => Err(self.fail(Error::Protocol)),
            Some(Reply::Packets(packets)) => {
                if Instant::now() >= packets.expires {
                    return Err(self.fail(Error::Expired));
                }
                for packet in &packets.units {
                    if self
                        .next
                        .is_some_and(|next| next != (packet.sequence(), packet.timestamp_samples()))
                    {
                        return Err(self.fail(Error::CaptureDiscontinuity));
                    }
                    self.next = Some((
                        packet
                            .sequence()
                            .checked_add(1)
                            .ok_or_else(|| self.fail(Error::Clock))?,
                        packet
                            .timestamp_samples()
                            .checked_add(u64::from(packet.duration_samples()))
                            .ok_or_else(|| self.fail(Error::Clock))?,
                    ));
                }
                self.phase = Phase::Idle;
                self.until = None;
                Ok(Some(Batch {
                    packets,
                    control: self.control(),
                }))
            }
        }
    }
    pub fn stop(&mut self) {
        self.shared.stop(Error::Stopped);
        self.phase = Phase::Stopped;
        self.until = None;
    }
}
impl Drop for MicrophoneCapture {
    fn drop(&mut self) {
        self.stop();
    }
}
fn gate(shared: &Shared, permitted: &mut impl FnMut() -> bool) -> Result<(), Error> {
    shared.check()?;
    if !catch_unwind(AssertUnwindSafe(permitted)).unwrap_or(false) {
        shared.stop(Error::Denied);
        return Err(Error::Denied);
    }
    shared.check()
}
