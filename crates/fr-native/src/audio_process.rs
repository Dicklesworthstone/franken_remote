#![forbid(unsafe_code)]
//! Explicit microphone capture in one supervised native child.
//!
//! The caller supplies the original session's durable, nonblocking permission
//! monitor and absolute authorization deadline. The supervisor checks both even
//! when the caller stops polling. A terminal private channel also withdraws the
//! child's permission; no request, replacement or retained batch can revive it.
//! Process separation contains native hangs/crashes, not a same-user sandbox.
//! The optional network coordinator retains the original authenticated attachment.
//! There is no default source or automatic enable.
pub mod child;
#[cfg(feature = "microphone-uplink")]
pub mod network;
#[cfg(target_os = "macos")]
pub mod permission;
#[cfg(any(test, target_os = "macos"))]
pub(crate) mod permission_protocol;
mod protocol;
mod supervisor;
pub mod transmit;

use crate::clock::CaptureDeadline as Deadline;
pub use crate::microphone_selection::Selection;
#[cfg(target_os = "linux")]
use crate::pulse::Error as PulseError;
const MAX_CAPTURE_CUSTODY: Duration = Duration::from_millis(100);
const MAX_CAPTURE_PACKETS: usize = 4;
use fr_core::audio::{AudioDirection, AudioStreamConfig, MAX_OPUS_PAYLOAD_BYTES};
use fr_media::{audio::AudioAccessUnit, worker::audio::CaptureCounters};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Path, PathBuf},
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
#[cfg(target_os = "linux")]
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
    pub selection: Selection,
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
        if !self.selection.valid() || !self.selection.platform_available() {
            return Err(Error::Selection);
        }
        // The first CoreAudio slice has one exact PCM/Opus profile. Unsupported
        // formats refuse before a native device or worker can be allocated.
        if matches!(self.selection, Selection::Mac { .. })
            && (c.channels().count() != 1 || c.frame_duration_ms() != 10)
        {
            return Err(Error::Configuration);
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
    expires_ns: u64,
}
impl Packets {
    fn deadline(&self) -> Deadline {
        Deadline {
            until: self.expires,
            ns: self.expires_ns,
        }
    }
}
impl Deadline {
    fn after(duration: Duration) -> Result<Self, Error> {
        let raw = crate::clock::suspend_inclusive_ns().ok_or(Error::Clock)?;
        Self::after_at(Instant::now(), raw, duration)
    }
    fn after_at(now: Instant, raw: u64, duration: Duration) -> Result<Self, Error> {
        Ok(Self {
            until: now.checked_add(duration).ok_or(Error::Clock)?,
            ns: raw
                .checked_add(u64::try_from(duration.as_nanos()).map_err(|_| Error::Clock)?)
                .ok_or(Error::Clock)?,
        })
    }
    fn remaining(self) -> Result<Duration, Error> {
        self.remaining_at(
            Instant::now(),
            crate::clock::suspend_inclusive_ns().ok_or(Error::Clock)?,
        )
    }
    fn remaining_at(self, now: Instant, raw: u64) -> Result<Duration, Error> {
        self.duration_at(now, raw).ok_or(Error::Expired)
    }
    fn live(self) -> bool {
        self.remaining().is_ok()
    }
}
enum Request {
    Pull(Deadline),
    Renew {
        authorization: Deadline,
        expires: Deadline,
    },
}
impl Request {
    fn deadline(&self) -> Deadline {
        match self {
            Self::Pull(deadline)
            | Self::Renew {
                expires: deadline, ..
            } => *deadline,
        }
    }
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
    authorization: Mutex<Deadline>,
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
    fn authorization(&self) -> Result<Deadline, Error> {
        self.authorization
            .lock()
            .map(|authorization| *authorization)
            .map_err(|_| Error::Stopped)
    }
    fn install_authorization(
        &self,
        old_ns: u64,
        next: Deadline,
        exchange: Deadline,
    ) -> Result<(), Error> {
        self.check_until(Some(exchange))?;
        {
            let mut current = self.authorization.lock().map_err(|_| Error::Stopped)?;
            if current.ns != old_ns || self.stopped.load(Ordering::Acquire) {
                return Err(Error::Stopped);
            }
            // The child acknowledgement, not request enqueue, commits this
            // future cap. A delayed acknowledgement cannot revive the old one.
            if !current.live() || !next.live() || !exchange.live() {
                return Err(Error::Expired);
            }
            *current = next;
        }
        self.check_until(Some(exchange))
    }
    fn check(&self) -> Result<(), Error> {
        self.check_until(None)
    }
    fn check_until(&self, deadline: Option<Deadline>) -> Result<(), Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(self.error());
        }
        // Application callbacks never execute while holding a mailbox lock.
        let before_callback = self.authorization()?;
        let error = if !before_callback.live() || deadline.is_some_and(|cap| !cap.live()) {
            Some(Error::Expired)
        } else if !catch_unwind(AssertUnwindSafe(|| (self.monitor)())).unwrap_or(false) {
            Some(Error::Denied)
        } else if self.stopped.load(Ordering::Acquire) {
            Some(self.error())
        } else if !self.authorization()?.live() || deadline.is_some_and(|cap| !cap.live()) {
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
    fn reply(&self, reply: Reply, deadline: Deadline) -> Result<(), Error> {
        self.check_until(Some(deadline))?;
        let mut mail = self.mail.lock().map_err(|_| Error::Stopped)?;
        if self.stopped.load(Ordering::Acquire) || mail.reply.is_some() {
            return Err(Error::Stopped);
        }
        if !deadline.live() {
            return Err(Error::Expired);
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
        self.0.stopped.load(Ordering::Acquire) || !self.0.authorization().is_ok_and(Deadline::live)
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
    /// Original private same-boot deadline, including suspend on Darwin.
    pub const fn expires_ns(&self) -> u64 {
        self.packets.expires_ns
    }
    pub const fn counters(&self) -> CaptureCounters {
        self.packets.counters
    }
    pub fn consume(
        self,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Vec<AudioAccessUnit>, Error> {
        gate_until(
            &self.control.0,
            Some(self.packets.deadline()),
            &mut permitted,
        )?;
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
    until: Option<Deadline>,
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
        let authorization_ns = crate::clock::stamp_ns(authorization_until).ok_or(Error::Clock)?;
        Self::start_with_deadline(
            image,
            options,
            authorization_until,
            authorization_ns,
            monitor,
        )
    }
    /// Keep the original native-clock authorization supplied by the same
    /// controller. The public compatibility entry point stamps only once.
    pub(crate) fn start_with_deadline(
        image: &Path,
        options: CaptureOptions,
        authorization_until: Instant,
        authorization_ns: u64,
        monitor: impl Fn() -> bool + Send + Sync + 'static,
    ) -> Result<Self, Error> {
        let authorization = Deadline {
            until: authorization_until,
            ns: authorization_ns,
        };
        // Capture setup before validation, filesystem access or application
        // callbacks; neither a delayed spawn nor suspend can restart its cap.
        let until = Deadline::after(SETUP)?.narrow(authorization);
        options.validate()?;
        let shared = Arc::new(Shared {
            mail: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            retired: AtomicBool::new(true),
            pid: AtomicU32::new(0),
            authorization: Mutex::new(authorization),
            monitor: Arc::new(monitor),
        });
        shared.check_until(Some(until))?;
        if !image.is_absolute() || !image.is_file() {
            return Err(Error::Unavailable);
        }
        let slot = Slot::take()?;
        let config = options.configuration;
        shared.check_until(Some(until))?;
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
        gate_until(&self.shared, self.until, permitted)?;
        let reply = self
            .shared
            .mail
            .lock()
            .map_err(|_| Error::Stopped)?
            .reply
            .take();
        gate_until(&self.shared, self.until, permitted)?;
        Ok(reply)
    }
    pub fn poll_ready(&mut self, mut permitted: impl FnMut() -> bool) -> Result<bool, Error> {
        gate_until(&self.shared, self.until, &mut permitted)?;
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
        permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let expires_ns = crate::clock::stamp_ns(expires).ok_or(Error::Clock)?;
        self.request_with_deadline(expires, expires_ns, permitted)
    }
    pub(crate) fn request_with_deadline(
        &mut self,
        expires: Instant,
        expires_ns: u64,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let maximum = Deadline::after(MAX_CAPTURE_CUSTODY)?;
        let expires = Deadline {
            until: expires,
            ns: expires_ns,
        };
        gate_until(&self.shared, Some(expires), &mut permitted)?;
        if self.phase != Phase::Idle {
            return Err(Error::Backpressure);
        }
        if expires.until > maximum.until || expires.ns > maximum.ns {
            return Err(self.fail(Error::Configuration));
        }
        let expires = expires.narrow(self.shared.authorization()?);
        {
            let mut mail = self.shared.mail.lock().map_err(|_| Error::Stopped)?;
            if self.shared.stopped.load(Ordering::Acquire) {
                return Err(mail.error.unwrap_or(Error::Stopped));
            }
            if mail.request.is_some() || mail.reply.is_some() {
                return Err(Error::Backpressure);
            }
            if !expires.live() {
                drop(mail);
                return Err(self.fail(Error::Expired));
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
        permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let ns = crate::clock::stamp_ns(authorization_until).ok_or(Error::Clock)?;
        self.renew_authority_with_deadline(authorization_until, ns, permitted)
    }
    pub(crate) fn renew_authority_with_deadline(
        &mut self,
        authorization_until: Instant,
        ns: u64,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        let authorization = Deadline {
            until: authorization_until,
            ns,
        };
        let expires = Deadline::after(RENEWAL_EXCHANGE)?.narrow(authorization);
        gate_until(&self.shared, Some(expires), &mut permitted)?;
        if self.phase != Phase::Idle {
            return Err(Error::Backpressure);
        }
        let current = self.shared.authorization()?;
        if ns == current.ns {
            return Err(Error::Configuration);
        }
        let expires = expires.narrow(current);
        {
            let mut mail = self.shared.mail.lock().map_err(|_| Error::Stopped)?;
            if self.shared.stopped.load(Ordering::Acquire) {
                return Err(mail.error.unwrap_or(Error::Stopped));
            }
            if mail.request.is_some() || mail.reply.is_some() {
                return Err(Error::Backpressure);
            }
            if !expires.live() {
                drop(mail);
                return Err(self.fail(Error::Expired));
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
        gate_until(&self.shared, self.until, &mut permitted)?;
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
        gate_until(&self.shared, self.until, &mut permitted)?;
        if self.phase != Phase::Pulling {
            return Err(Error::Backpressure);
        }
        match self.collect(&mut permitted)? {
            None => Ok(None),
            Some(Reply::Ready | Reply::Renewed) => Err(self.fail(Error::Protocol)),
            Some(Reply::Packets(packets)) => {
                if !packets.deadline().live() {
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
fn gate_until(
    shared: &Shared,
    deadline: Option<Deadline>,
    permitted: &mut impl FnMut() -> bool,
) -> Result<(), Error> {
    shared.check_until(deadline)?;
    if !catch_unwind(AssertUnwindSafe(permitted)).unwrap_or(false) {
        shared.stop(Error::Denied);
        return Err(Error::Denied);
    }
    shared.check_until(deadline)
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use fr_core::audio::{AudioChannels, AudioGeneration};

    pub(super) fn shared(monitor: impl Fn() -> bool + Send + Sync + 'static) -> Arc<Shared> {
        Arc::new(Shared {
            mail: Mutex::default(),
            wake: Condvar::new(),
            stopped: AtomicBool::new(false),
            retired: AtomicBool::new(true),
            pid: AtomicU32::new(0),
            authorization: Mutex::new(Deadline::after(Duration::from_secs(3600)).unwrap()),
            monitor: Arc::new(monitor),
        })
    }

    fn options() -> CaptureOptions {
        CaptureOptions {
            #[cfg(target_os = "linux")]
            selection: Selection::Pulse {
                server: "/test/private-pulse".into(),
                source: "test".into(),
            },
            #[cfg(target_os = "macos")]
            selection: Selection::Mac {
                uid: "test-device".into(),
            },
            configuration: AudioStreamConfig::new(
                AudioDirection::Uplink,
                AudioGeneration::from_raw(1),
                AudioChannels::Mono,
                10,
                20,
            )
            .unwrap(),
            bitrate: 64_000,
            max_packet_bytes: 512,
        }
    }

    fn idle(shared: Arc<Shared>) -> MicrophoneCapture {
        MicrophoneCapture {
            shared,
            config: options().configuration,
            phase: Phase::Idle,
            until: None,
            next: None,
        }
    }

    #[test]
    fn queued_deadlines_survive_suspend_without_reconstruction() {
        let start = Instant::now();
        let original = Deadline::after_at(start, 1_000_000_000, MAX_CAPTURE_CUSTODY).unwrap();
        let request = Request::Pull(original);
        // Darwin uptime advanced 5 ms while its native clock advanced 200 ms.
        // A fresh conversion of the stored Instant would incorrectly add 95 ms.
        let resumed = start + Duration::from_millis(5);
        assert_eq!(request.deadline(), original);
        assert_eq!(
            request.deadline().remaining_at(resumed, 1_200_000_000),
            Err(Error::Expired)
        );
        assert_eq!(
            original.remaining_at(start, original.ns),
            Err(Error::Expired)
        );
        assert_eq!(
            original.remaining_at(original.until, 1_000_000_000),
            Err(Error::Expired)
        );
    }

    #[test]
    fn independent_caps_and_waits_always_use_the_earlier_clock() {
        let start = Instant::now();
        let first = Deadline {
            until: start + Duration::from_millis(80),
            ns: 1_040_000_000,
        };
        let second = Deadline {
            until: start + Duration::from_millis(20),
            ns: 1_090_000_000,
        };
        let retained = first.narrow(second);
        assert_eq!(retained.until, second.until);
        assert_eq!(retained.ns, first.ns);
        assert_eq!(
            retained.remaining_at(start, 1_000_000_000),
            Ok(Duration::from_millis(20))
        );
        assert_eq!(
            retained.remaining_at(start, 1_039_000_000),
            Ok(Duration::from_millis(1))
        );
        assert_eq!(
            Deadline::after_at(start, u64::MAX, SETUP),
            Err(Error::Clock)
        );
    }

    #[test]
    fn expired_native_setup_refuses_before_monitor_or_spawn() {
        let error = MicrophoneCapture::start_with_deadline(
            Path::new("/no-native-child-can-start"),
            options(),
            Instant::now() + Duration::from_secs(3600),
            1,
            || panic!("an expired original setup cannot consult permission"),
        )
        .unwrap_err();
        assert_eq!(error, Error::Expired);
    }

    #[test]
    fn expired_native_pull_and_renewal_never_enter_the_mailbox() {
        for renew in [false, true] {
            let shared = shared(|| panic!("raw expiry precedes every callback"));
            let original = shared.authorization().unwrap();
            let mut capture = idle(shared.clone());
            let local = Instant::now() + Duration::from_millis(80);
            let result = if renew {
                capture.renew_authority_with_deadline(local, 1, || true)
            } else {
                capture.request_with_deadline(local, 1, || true)
            };
            assert_eq!(result, Err(Error::Expired));
            assert!(shared.mail.lock().unwrap().request.is_none());
            assert_eq!(shared.authorization().unwrap(), original);
        }
    }

    #[test]
    fn late_native_ack_cannot_install_a_future_authorization() {
        let shared = shared(|| panic!("expired ACK cannot consult permission"));
        let original = shared.authorization().unwrap();
        let next = Deadline {
            until: original.until + SETUP,
            ns: original.ns + 2_000_000_000,
        };
        let exchange = Deadline {
            until: original.until,
            ns: 1,
        };
        assert_eq!(
            shared.install_authorization(original.ns, next, exchange),
            Err(Error::Expired)
        );
        assert_eq!(shared.authorization().unwrap(), original);
    }

    #[test]
    fn a_positive_callback_cannot_hide_native_authority_expiry() {
        let shared = shared(|| true);
        let deadline = shared.authorization().unwrap();
        let mut called = false;
        assert_eq!(
            gate_until(&shared, Some(deadline), &mut || {
                called = true;
                // The local Instant remains live; model the native cap expiring
                // during an otherwise positive application callback.
                shared.authorization.lock().unwrap().ns = 1;
                true
            }),
            Err(Error::Expired)
        );
        assert!(called);
        assert!(shared.stopped.load(Ordering::Acquire));
    }
}
