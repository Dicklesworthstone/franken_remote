//! Bounded per-epoch Opus child with a native IPC supervisor, NOT an async
//! runtime. The client only exchanges one mailbox slot; it never waits for
//! process spawn, codec execution, socket I/O, child termination or reaping.
//! At most four supervisors exist, including cancelled-but-not-reaped owners.
//! No automatic worker restart, native fallback or unbounded reaper queue.
pub mod child;
mod protocol;
mod supervisor;
use super::CodecLimits;
use fr_client::audio::playout::decoder::PolledDecoder;
use fr_core::audio::AudioStreamConfig;
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
const SETUP: Duration = Duration::from_secs(2);
const DECODE: Duration = Duration::from_millis(100);
const MAX_WORKERS: usize = 4;
static WORKERS: AtomicUsize = AtomicUsize::new(0);
struct Slot;
impl Slot {
    fn take() -> Result<Self, AudioMediaError> {
        WORKERS
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < MAX_WORKERS).then_some(n + 1)
            })
            .map(|_| Self)
            .map_err(|_| AudioMediaError::Backpressure)
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        WORKERS.fetch_sub(1, Ordering::AcqRel);
    }
}
enum Job {
    Packet(Box<AudioAccessUnit>),
    Plc(u16),
}
enum Reply {
    Ready,
    Pcm(Box<AudioPcmFrame>),
}
impl Drop for Reply {
    fn drop(&mut self) {
        if let Self::Pcm(pcm) = self {
            pcm.samples_mut().fill(0);
        }
    }
}
#[derive(Default)]
struct Mailbox {
    job: Option<(Job, Instant)>,
    reply: Option<Result<Reply, AudioMediaError>>,
    stop: bool,
}
struct Shared {
    mail: Mutex<Mailbox>,
    wake: Condvar,
    retired: AtomicBool,
    pid: AtomicU32,
}
impl Shared {
    fn stop(&self) {
        if let Ok(mut m) = self.mail.lock() {
            m.stop = true;
            m.job = None;
            m.reply = None;
        }
        self.wake.notify_one();
    }
    fn reply(&self, result: Result<Reply, AudioMediaError>) {
        if let Ok(mut m) = self.mail.lock()
            && !m.stop
        {
            m.reply = Some(result);
        }
    }
}
/// Retain independently to distinguish requested termination from actual child
/// destruction. A new epoch never reuses this child. No raw process access.
#[derive(Clone)]
pub struct Retirement(Arc<Shared>);
impl Retirement {
    pub fn is_complete(&self) -> bool {
        self.0.retired.load(Ordering::Acquire)
    }
    pub fn stop(&self) {
        self.0.stop();
    }
    /// Local PID for native supervision/diagnostics, never sent to the peer.
    pub fn process_id(&self) -> Option<u32> {
        let id = self.0.pid.load(Ordering::Acquire);
        (id != 0).then_some(id)
    }
}
impl std::fmt::Debug for Retirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusRetirement")
            .field("complete", &self.is_complete())
            .finish_non_exhaustive()
    }
}
#[derive(PartialEq, Eq)]
enum Phase {
    New,
    Configuring,
    Idle,
    Decoding,
    Stopped,
}
pub struct ProcessDecoder {
    image: PathBuf,
    limits: CodecLimits,
    shared: Arc<Shared>,
    phase: Phase,
    config: Option<AudioStreamConfig>,
    until: Option<Instant>,
}
impl std::fmt::Debug for ProcessDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProcessOpusDecoder([private per-epoch owner])")
    }
}
impl ProcessDecoder {
    /// The path is selected locally and must be an installed absolute image.
    /// Native process creation happens only after configure on its supervisor.
    pub fn new(image: &Path, limits: CodecLimits) -> Result<(Self, Retirement), AudioMediaError> {
        if !image.is_absolute() || !image.is_file() {
            return Err(AudioMediaError::UnsupportedFormat);
        }
        let shared = Arc::new(Shared {
            mail: Mutex::new(Mailbox::default()),
            wake: Condvar::new(),
            retired: AtomicBool::new(true),
            pid: AtomicU32::new(0),
        });
        let receipt = Retirement(shared.clone());
        Ok((
            Self {
                image: image.into(),
                limits,
                shared,
                phase: Phase::New,
                config: None,
                until: None,
            },
            receipt,
        ))
    }
    fn fail(&mut self, error: AudioMediaError) -> AudioMediaError {
        self.shared.stop();
        self.phase = Phase::Stopped;
        error
    }
    fn collect(&mut self) -> Result<Option<Reply>, AudioMediaError> {
        if self.phase == Phase::Stopped {
            return Err(AudioMediaError::Fatal);
        }
        if self.until.is_some_and(|t| Instant::now() >= t) {
            return Err(self.fail(AudioMediaError::Fatal));
        }
        let response = {
            let mut m = self
                .shared
                .mail
                .lock()
                .map_err(|_| AudioMediaError::Fatal)?;
            if m.stop {
                return Err(AudioMediaError::Fatal);
            }
            m.reply.take()
        };
        match response {
            Some(Ok(reply)) => {
                self.until = None;
                Ok(Some(reply))
            }
            Some(Err(error)) => Err(self.fail(error)),
            None => Ok(None),
        }
    }
    fn enqueue(&mut self, job: Job) -> Result<(), AudioMediaError> {
        if self.phase != Phase::Idle {
            return Err(AudioMediaError::Backpressure);
        }
        let until = Instant::now() + DECODE;
        {
            let mut m = self
                .shared
                .mail
                .lock()
                .map_err(|_| AudioMediaError::Fatal)?;
            if m.stop {
                return Err(AudioMediaError::Fatal);
            }
            if m.job.is_some() || m.reply.is_some() {
                return Err(AudioMediaError::Backpressure);
            }
            m.job = Some((job, until));
        }
        self.until = Some(until);
        self.phase = Phase::Decoding;
        self.shared.wake.notify_one();
        Ok(())
    }
}
impl PolledDecoder for ProcessDecoder {
    fn configure(&mut self, c: AudioStreamConfig) -> Result<(), AudioMediaError> {
        if self
            .shared
            .mail
            .lock()
            .map_err(|_| AudioMediaError::Fatal)?
            .stop
        {
            return Err(AudioMediaError::Fatal);
        }
        if self.phase != Phase::New {
            return Err(AudioMediaError::InvalidPayload);
        }
        if c.expected_samples_per_frame() > self.limits.max_decoded_samples() {
            return Err(AudioMediaError::BufferOverflow);
        }
        let slot = Slot::take()?;
        let until = Instant::now() + SETUP;
        self.shared.retired.store(false, Ordering::Release);
        let (image, limits, shared) = (self.image.clone(), self.limits, self.shared.clone());
        let spawned = std::thread::Builder::new()
            .name("fr-opus-owner".into())
            .spawn(move || {
                supervisor::run(image, c, limits, until, &shared, slot);
            });
        if spawned.is_err() {
            self.shared.retired.store(true, Ordering::Release);
            return Err(self.fail(AudioMediaError::Fatal));
        }
        self.phase = Phase::Configuring;
        self.config = Some(c);
        self.until = Some(until);
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        if self
            .shared
            .mail
            .lock()
            .map_err(|_| AudioMediaError::Fatal)?
            .stop
        {
            return Err(AudioMediaError::Fatal);
        }
        if matches!(self.phase, Phase::Idle | Phase::Decoding) {
            return Ok(true);
        }
        if self.phase != Phase::Configuring {
            return Err(AudioMediaError::NotConfigured);
        }
        match self.collect()? {
            Some(Reply::Ready) => {
                self.phase = Phase::Idle;
                Ok(true)
            }
            Some(_) => Err(self.fail(AudioMediaError::InvalidPayload)),
            None => Ok(false),
        }
    }
    fn submit_packet(&mut self, p: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        let c = self.config.ok_or(AudioMediaError::NotConfigured)?;
        if p.generation() != c.generation()
            || p.direction() != c.direction()
            || u32::from(p.duration_samples()) != c.expected_samples_per_frame()
            || p.payload().len() > self.limits.max_packet_bytes()
        {
            return Err(AudioMediaError::InvalidPayload);
        }
        self.enqueue(Job::Packet(Box::new(*p)))
    }
    fn submit_plc(&mut self, samples: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        if self
            .config
            .is_none_or(|c| c.expected_samples_per_frame() != u32::from(samples))
        {
            return Err(AudioMediaError::InvalidPayload);
        }
        self.enqueue(Job::Plc(samples))?;
        Ok(None)
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        if self.phase == Phase::Idle {
            return Ok(None);
        }
        if self.phase != Phase::Decoding {
            return Err(AudioMediaError::NotConfigured);
        }
        match self.collect()? {
            Some(mut r) => {
                let Reply::Pcm(p) = &mut r else {
                    return Err(self.fail(AudioMediaError::InvalidPayload));
                };
                let result = **p;
                self.phase = Phase::Idle;
                Ok(Some(result))
            }
            None => Ok(None),
        }
    }
}
impl Drop for ProcessDecoder {
    fn drop(&mut self) {
        self.shared.stop();
    }
}
