#![forbid(unsafe_code)]
//! Explicit client microphone RECORD → real Opus, for a supervised native
//! worker. The caller selects one local source and supplies the original
//! session's talk-enable, OS-permission and controller-authority check.
//! Connecting elsewhere, playback enable, or a new generation never grants it.
//!
//! Capture and codec calls stay on the creating worker thread. Native work is
//! bounded and checked before/after; a foreign-library stall still requires
//! process supervision. A batch keeps its original local expiry, and partial
//! PCM cannot acquire a renewed expiry by surviving another pull. No transport,
//! physical-device qualification, default microphone, or automatic resume is
//! implied by this API.
use super::{Error, capture::CaptureSelection, source::RecordSource};
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_media::{audio::AudioAccessUnit, worker::audio::CaptureCounters};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Maximum original request custody, independent of the native queue bound.
pub const MAX_CAPTURE_CUSTODY: Duration = Duration::from_millis(100);
pub const MAX_CAPTURE_PACKETS: usize = super::source::PULL_FRAMES;
const STARTUP: Duration = Duration::from_secs(2);

/// Thread-safe terminal fence for one original capture owner. A clone cannot
/// keep the native connection alive or enable a replacement generation.
#[derive(Clone)]
pub struct Control(Arc<AtomicBool>);
impl Control {
    pub fn stop(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneCaptureControl")
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

/// At most four real Opus packets from one bounded native pull. Content remains
/// private until a final permission/expiry check at `consume`. The downstream
/// worker/transport must preserve `expires` and recheck its own send boundary.
pub struct Batch {
    packets: Vec<AudioAccessUnit>,
    counters: CaptureCounters,
    expires: Instant,
    control: Control,
}
impl Batch {
    pub const fn expires(&self) -> Instant {
        self.expires
    }
    pub const fn counters(&self) -> CaptureCounters {
        self.counters
    }
    pub fn packet_count(&self) -> usize {
        self.packets.len()
    }
    pub fn consume(
        self,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Vec<AudioAccessUnit>, Error> {
        check(&self.control, Some(self.expires), &mut permitted)?;
        Ok(self.packets)
    }
}
impl std::fmt::Debug for Batch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneCaptureBatch")
            .field("packets", &self.packet_count())
            .field("counters", &self.counters)
            .finish_non_exhaustive()
    }
}

/// A unique thread-confined native recording stream and Opus encoder. A device
/// change, permission loss, expiry or native error retires the original owner;
/// callers must acquire fresh permission and a fresh generation to replace it.
pub struct MicrophoneSource {
    source: Option<RecordSource>,
    config: AudioStreamConfig,
    control: Control,
    origin: Instant,
    ready: bool,
    partial: Option<(u64, Instant)>,
    next: Option<(u64, u64)>,
}
impl std::fmt::Debug for MicrophoneSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneSource")
            .field("ready", &self.ready)
            .field("stopped", &self.control.is_stopped())
            .finish_non_exhaustive()
    }
}
impl MicrophoneSource {
    /// The selected source name is local configuration, never remote input.
    /// `PulseAudio`'s authenticated record grant is required in addition to the
    /// caller's original permission; successful construction is not Ready.
    /// Permission predicates must be nonblocking. A panic denies and fences
    /// this original owner; it cannot resume if the caller catches unwinding.
    pub fn connect(
        server: &Path,
        source: &str,
        config: AudioStreamConfig,
        bitrate: u32,
        max_packet_bytes: u16,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Self, Error> {
        if config.direction() != AudioDirection::Uplink {
            return Err(Error::Configuration);
        }
        if server
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(Error::Selection);
        }
        let selection = CaptureSelection::microphone(server, source)?;
        let control = Control(Arc::new(AtomicBool::new(false)));
        check(&control, None, &mut permitted)?;
        let origin = Instant::now();
        let startup_until = origin.checked_add(STARTUP).ok_or(Error::Clock)?;
        let source = RecordSource::connect(selection, config, bitrate, max_packet_bytes, 0)?;
        let owner = Self {
            source: Some(source),
            config,
            control,
            origin,
            ready: false,
            partial: None,
            next: None,
        };
        check(&owner.control, Some(startup_until), &mut permitted)?;
        Ok(owner)
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub const fn configuration(&self) -> AudioStreamConfig {
        self.config
    }
    pub const fn is_ready(&self) -> bool {
        self.ready
    }

    /// Bounded native progress. The original permission is rechecked before
    /// starting the actual RECORD stream and after every native main-loop turn.
    pub fn poll_ready(&mut self, mut permitted: impl FnMut() -> bool) -> Result<bool, Error> {
        let source = self.source.as_mut().ok_or(Error::Closed)?;
        let mut now = || {
            check(&self.control, None, &mut permitted)?;
            elapsed(self.origin)
        };
        let result = source.poll_ready(&mut now).and_then(|ready| {
            now()?;
            Ok(ready)
        });
        match result {
            Ok(ready) => {
                self.ready = ready;
                Ok(ready)
            }
            Err(error) => {
                self.close();
                Err(error)
            }
        }
    }

    /// Drain the native record queue and encode its newest complete frames.
    /// `expires` is the ORIGINAL local request deadline, at most 100 ms ahead.
    /// Permission/deadline checks bracket every read and every codec call.
    /// An older partial frame retains its first deadline across pull calls.
    pub fn pull(
        &mut self,
        expires: Instant,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Batch, Error> {
        let source = self.source.as_mut().ok_or(Error::Closed)?;
        let result = (|| {
            check(&self.control, Some(expires), &mut permitted)?;
            if expires
                > Instant::now()
                    .checked_add(MAX_CAPTURE_CUSTODY)
                    .ok_or(Error::Clock)?
            {
                return Err(Error::Configuration);
            }
            let until = self.partial.map_or(expires, |(_, old)| old.min(expires));
            let mut now = || {
                check(&self.control, Some(until), &mut permitted)?;
                elapsed(self.origin)
            };
            let (packets, counters) = source.pull_contiguous(&mut now)?;
            now()?;
            self.next = sequence(self.config, &packets, self.next)?;
            self.partial = source.partial_timestamp().map(|timestamp| {
                (
                    timestamp,
                    self.partial
                        .filter(|(old, _)| *old == timestamp)
                        .map_or(expires, |(_, original)| original),
                )
            });
            Ok(Batch {
                packets,
                counters,
                expires: until,
                control: self.control.clone(),
            })
        })();
        if result.is_err() {
            self.close();
        }
        result
    }
    /// Fence before dropping native/codec ownership and erasing partial PCM.
    pub fn close(&mut self) {
        self.control.stop();
        self.ready = false;
        self.partial = None;
        self.next = None;
        self.source = None;
    }
}
impl Drop for MicrophoneSource {
    fn drop(&mut self) {
        self.close();
    }
}
fn check(
    control: &Control,
    until: Option<Instant>,
    permitted: &mut impl FnMut() -> bool,
) -> Result<(), Error> {
    // A caller may catch Rust unwinding and retain this owner. Turn an uncertain
    // permission check into terminal denial so neither capture nor an existing
    // encoded batch can resume afterward. No closure or panic crosses C.
    if control.is_stopped()
        || !catch_unwind(AssertUnwindSafe(permitted)).unwrap_or(false)
        || control.is_stopped()
    {
        control.stop();
        return Err(Error::Denied);
    }
    if until.is_some_and(|until| Instant::now() >= until) {
        return Err(Error::Expired);
    }
    Ok(())
}
fn elapsed(origin: Instant) -> Result<u64, Error> {
    u64::try_from(origin.elapsed().as_micros()).map_err(|_| Error::Clock)
}
fn sequence(
    config: AudioStreamConfig,
    packets: &[AudioAccessUnit],
    mut next: Option<(u64, u64)>,
) -> Result<Option<(u64, u64)>, Error> {
    if packets.len() > MAX_CAPTURE_PACKETS {
        return Err(Error::BufferLimit);
    }
    let samples = config.expected_samples_per_frame();
    for packet in packets {
        if packet.direction() != AudioDirection::Uplink
            || packet.generation() != config.generation()
            || u32::from(packet.duration_samples()) != samples
        {
            return Err(Error::Metadata);
        }
        if next.is_some_and(|next| next != (packet.sequence(), packet.timestamp_samples())) {
            return Err(Error::CaptureDiscontinuity);
        }
        next = Some((
            packet.sequence().checked_add(1).ok_or(Error::Clock)?,
            packet
                .timestamp_samples()
                .checked_add(u64::from(samples))
                .ok_or(Error::Clock)?,
        ));
    }
    Ok(next)
}
