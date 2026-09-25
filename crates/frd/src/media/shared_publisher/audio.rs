//! Host playback audio for ONE OS share (plan §15.4; PROTOCOL.md 0x0060-0x0063).
//!
//! `frd run --audio` is the local enable. It only arms a demand-driven source:
//! the separate `fr-media-worker --audio` process starts when at least one
//! admitted, streaming observer that selected audio-down is attached, and is
//! stopped and reaped when none remains. frd links neither libpulse nor
//! libopus; it receives already encoded packets over the bounded private IPC.
//!
//! Packets enter the share's single bounded [`AudioRing`]; each viewer's
//! [`AudioLane`] sends `AudioConfiguration`, waits for that viewer's matching
//! `AudioConfigured`, then sends only newer packets as datagrams on its own
//! audio-down route. Every send rechecks the source's and that viewer's
//! observation authority. Audio never touches frame IDs, source progress,
//! recovery or view readiness: it is not capture freshness evidence.
use super::{Entry, Error, Members, ObservationControl, SendReport};
use crate::{
    media_quic::AudioLanes,
    worker::{self, Deadline, Launch, Retirement, Worker},
};
use asupersync::{cx::Cx, time::sleep_until, types::Time};
use fr_core::audio::{AudioChannels, AudioGeneration, AudioStopReason};
use fr_media::{
    audio_delivery::{AudioLane, AudioRing, LaneAction, SourceStream},
    worker::{
        Kind, Role,
        audio::{Capture, Monitor, decode_batch},
    },
};
use fr_transport::quic::{self, Disposition, QuicRecords, Route};
use fr_wire::audio as wire;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};

/// Pull cadence: half of the 20 ms packet duration.
const PULL_US: u64 = 10_000;
/// Deadlines for the private worker exchanges (supervision, not latency).
const START_TIMEOUT: Duration = Duration::from_secs(3);
const PULL_TIMEOUT: Duration = Duration::from_millis(500);
const STOP_TIMEOUT: Duration = Duration::from_secs(1);
/// Reliable-record retention for configuration and stop records.
const CONTROL_SEND_US: u64 = 2_000_000;
/// Admission window for one packet datagram.
const PACKET_SEND_US: u64 = 40_000;
/// Records per viewer service turn: bounded, no catch-up burst.
const RECORDS_PER_TURN: usize = 4;
const FRAME_MS: u16 = 20;
const JITTER_MS: u16 = 20;
const BITRATE: u32 = 96_000;
/// Opus payload ceiling: an `AudioPacket` record must fit the 1150-byte
/// negotiated datagram cap (56 bytes of record overhead).
pub const MAX_PACKET_BYTES: u16 = 1000;

/// The operator's local audio choices. Never peer-selectable.
#[derive(Clone)]
pub struct AudioProfile {
    image: PathBuf,
    display: String,
    server: String,
    monitor: Monitor,
    channels: AudioChannels,
    /// Qualified host randomness for each child's private IPC epoch.
    entropy: crate::session_startup::shared_viewers::Entropy,
    /// The current audio child's launch receipt, for the share's cleanup.
    retired: Arc<Mutex<Option<Retirement>>>,
}
impl std::fmt::Debug for AudioProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioProfile")
            .field("monitor", &self.monitor)
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}
impl AudioProfile {
    /// `image` is the absolute worker image; `display` only satisfies the
    /// private launch contract (the audio role opens no X connection).
    pub fn new(
        image: PathBuf,
        display: String,
        server: String,
        monitor: Monitor,
        entropy: crate::session_startup::shared_viewers::Entropy,
        retired: Arc<Mutex<Option<Retirement>>>,
    ) -> Result<Self, Error> {
        let profile = Self {
            image,
            display,
            server,
            monitor,
            channels: AudioChannels::Stereo,
            entropy,
            retired,
        };
        // Validate the complete worker configuration once, up front.
        profile
            .capture(AudioGeneration::from_raw(1))
            .validate()
            .map_err(|_| Error::InvalidBudget)?;
        if !profile.image.is_absolute() {
            return Err(Error::InvalidBudget);
        }
        Ok(profile)
    }
    fn capture(&self, generation: AudioGeneration) -> Capture {
        Capture {
            generation,
            channels: self.channels,
            frame_duration_ms: FRAME_MS,
            bitrate: BITRATE,
            max_packet_bytes: MAX_PACKET_BYTES,
            server: self.server.clone(),
            monitor: self.monitor.clone(),
        }
    }
    fn stream(&self, generation: AudioGeneration) -> SourceStream {
        SourceStream {
            generation,
            channels: self.channels,
            frame_duration_ms: FRAME_MS,
            max_packet_bytes: MAX_PACKET_BYTES,
            jitter_target_ms: JITTER_MS,
        }
    }
}

/// Per-viewer audio state, bound to the exact attachment it was created on.
#[derive(Debug, Default)]
pub(super) struct EntryAudio {
    lane: AudioLane,
    lanes: Option<AudioLanes>,
}
impl EntryAudio {
    /// Departure/teardown fence: no further configuration, packet or stop.
    pub(super) fn close(&mut self) {
        self.lane.close();
    }
}
impl Entry {
    /// Admitted, decoder-ready observation with an attached audio-down lane.
    pub(super) fn audio_streaming(&self) -> bool {
        self.failure.is_none()
            && self.join.is_none()
            && self.starting.is_none()
            && self.recovery.is_none()
            && self
                .media
                .as_ref()
                .is_some_and(crate::media_quic::NegotiatedMedia::audio_selected)
    }
    fn audio_demand(&self) -> bool {
        self.audio_streaming() && self.audio.lanes.is_some() && self.audio.lane.wants_audio()
    }
    /// Bounded: at most `RECORDS_PER_TURN` audio records. A broken audio lane
    /// fences audio only; it never ends this viewer's video.
    pub(super) fn service_audio(
        &mut self,
        cx: &Cx,
        transport: &mut QuicRecords,
        ring: &AudioRing,
        owner: &ObservationControl,
        report: &mut SendReport,
    ) -> Result<(), Error> {
        if !self.audio_streaming() || self.audio.lane.is_stopped() {
            return Ok(());
        }
        let media = self.media.as_ref().ok_or(Error::Closed)?;
        let lanes = match media.audio_lanes(transport) {
            Ok(Some(lanes)) => lanes,
            Ok(None) => return Ok(()),
            Err(_) => {
                // Retired/reset audio lane: typed local fence, video continues.
                self.audio.lane.close();
                self.audio.lanes = None;
                return Ok(());
            }
        };
        // A lane that changed under us, or whose datagram route cannot carry
        // this source's largest packet, is a typed audio-only refusal.
        if self.audio.lanes.is_some_and(|l| l != lanes)
            || wire::packet_record_bytes(usize::from(MAX_PACKET_BYTES))
                .is_none_or(|n| n > lanes.packet_maximum)
        {
            self.audio.lane.close();
            return Ok(());
        }
        self.audio.lanes = Some(lanes);
        let control = self.control.clone();
        let mut authorize = || owner.check().is_ok() && control.check().is_ok();
        for _ in 0..RECORDS_PER_TURN {
            let now = owner.check().map_err(Error::Media)?.as_micros();
            let lane = &mut self.audio.lane;
            let action = lane.next(ring, now);
            let (route, record, deadline) = match audio_record(action, ring, lanes, now) {
                Ok(Some(send)) => send,
                Ok(None) => return Ok(()),
                // Never a video failure: fence this viewer's audio only.
                Err(_) => {
                    lane.close();
                    return Ok(());
                }
            };
            match transport.send(cx, route, &record, deadline, &mut authorize) {
                Ok(()) => {
                    report.accepted += 1;
                    let result = match action {
                        LaneAction::Configure(_) => lane.configuration_sent(now),
                        LaneAction::Stop(_) => lane.stop_sent(),
                        LaneAction::Packet { sequence, .. } => lane.packet_sent(sequence),
                        LaneAction::Nothing => Ok(()),
                    };
                    if result.is_err() {
                        lane.close();
                        return Ok(());
                    }
                }
                // Congestion: nothing is queued here; the packet ages out.
                Err(quic::Error::Backpressure) => {
                    report.pending = true;
                    return Ok(());
                }
                Err(error) => {
                    return Err(Error::Transport(crate::media_quic::Error::Transport(error)));
                }
            }
        }
        Ok(())
    }
    /// One reliable record on this viewer's audio reply lane. Records for
    /// other routes are not ours (`None`); a stale generation is refused and
    /// changes nothing; a malformed record fences audio only.
    pub(super) fn audio_record(
        &mut self,
        ring: &AudioRing,
        route: Route,
        bytes: &[u8],
    ) -> Option<Disposition> {
        let lanes = self.audio.lanes?;
        if route != Route::Stream(lanes.replies) {
            return None;
        }
        let lane = &mut self.audio.lane;
        match wire::record_kind(bytes) {
            Some(0x0061) => match wire::decode_configured(bytes, lanes.binding) {
                Ok(ack) => {
                    let _ = lane.configured(ack, ring);
                }
                Err(_) => lane.close(),
            },
            Some(0x0063) => match wire::decode_stop(bytes, lanes.binding) {
                Ok(stop) => {
                    let _ = lane.viewer_stopped(stop);
                }
                Err(_) => lane.close(),
            },
            _ => lane.close(),
        }
        Some(Disposition::Consumed)
    }
}

/// The exact record, route and send deadline for one lane action.
fn audio_record(
    action: LaneAction,
    ring: &AudioRing,
    lanes: AudioLanes,
    now: u64,
) -> Result<Option<(Route, Vec<u8>, u64)>, Error> {
    let control = |record: Vec<u8>| {
        Some((
            Route::Stream(lanes.control),
            record,
            now.saturating_add(CONTROL_SEND_US),
        ))
    };
    Ok(match action {
        LaneAction::Nothing => None,
        LaneAction::Configure(configuration) => {
            let mut record = vec![0; wire::AUDIO_CONFIGURATION_RECORD_BYTES];
            wire::encode_configuration(&configuration, lanes.binding, &mut record)
                .map_err(|_| Error::InvalidBudget)?;
            control(record)
        }
        LaneAction::Stop(stop) => {
            let mut record = vec![0; wire::AUDIO_STOP_RECORD_BYTES];
            wire::encode_stop(&stop, lanes.binding, &mut record)
                .map_err(|_| Error::InvalidBudget)?;
            control(record)
        }
        LaneAction::Packet {
            sequence,
            generation,
        } => {
            let packet = ring.get(sequence).ok_or(Error::Closed)?.unit;
            let bytes = wire::packet_record_bytes(packet.payload().len())
                .filter(|&n| n <= lanes.packet_maximum)
                .ok_or(Error::InvalidBudget)?;
            let mut record = vec![0; bytes];
            wire::encode_packet(
                &wire::AudioPacket {
                    direction: packet.direction(),
                    // The viewer's own lane epoch, never the private source one.
                    generation,
                    sequence: packet.sequence(),
                    timestamp_samples: packet.timestamp_samples(),
                    duration_samples: packet.duration_samples(),
                    payload: packet.payload(),
                },
                lanes.binding,
                &mut record,
            )
            .map_err(|_| Error::InvalidBudget)?;
            Some((
                Route::Datagram(lanes.packets),
                record,
                now.saturating_add(PACKET_SEND_US),
            ))
        }
    })
}

impl Members {
    pub(super) fn audio_demand(&self) -> bool {
        self.entries.iter().flatten().any(Entry::audio_demand)
    }
}

/// The source side's handle on the share's members: demand, clock, ring.
#[derive(Clone)]
pub struct AudioFeed {
    members: Weak<Mutex<Members>>,
}
impl std::fmt::Debug for AudioFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AudioFeed([share members])")
    }
}
impl AudioFeed {
    pub(super) const fn new(members: Weak<Mutex<Members>>) -> Self {
        Self { members }
    }
    fn with<T>(&self, action: impl FnOnce(&mut Members) -> T) -> Result<T, Error> {
        let shared = self.members.upgrade().ok_or(Error::Closed)?;
        let mut members = shared.lock().map_err(|_| Error::Poisoned)?;
        if members.closed {
            return Err(Error::Closed);
        }
        Ok(action(&mut members))
    }
    fn owner(&self) -> Result<ObservationControl, Error> {
        self.with(|m| m.owner.clone())
    }
}

/// The demand-driven source loop for one share. Dropping it (share teardown)
/// kills the live audio child; its launch receipt stays in the profile's slot
/// for the share's cleanup to reap.
pub struct AudioSource {
    profile: AudioProfile,
    feed: AudioFeed,
    worker: Option<Worker>,
    stream: Option<SourceStream>,
    next_generation: u64,
    /// A worker exists whose exit has not been confirmed: never start another.
    unreaped: bool,
    /// Typed terminal state for this share (unavailable source or failure).
    failed: bool,
}
impl std::fmt::Debug for AudioSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioSource")
            .field("live", &self.worker.is_some())
            .field("failed", &self.failed)
            .finish_non_exhaustive()
    }
}
impl AudioSource {
    pub fn new(profile: AudioProfile, feed: AudioFeed) -> Self {
        Self {
            profile,
            feed,
            worker: None,
            stream: None,
            next_generation: 1,
            unreaped: false,
            failed: false,
        }
    }
    /// Run until the share closes. Errors are the share's own closure; audio
    /// failures are typed per-viewer stops, never a share failure.
    pub async fn serve(mut self) -> Result<(), Error> {
        loop {
            let owner = self.feed.owner()?;
            let cx = owner.context();
            let now = owner.check().map_err(Error::Media)?.as_micros();
            let demand = self.feed.with(|m| m.audio_demand())?;
            match (self.worker.is_some(), demand) {
                (false, true) if !self.failed && !self.unreaped => self.start(&cx).await?,
                (true, true) => self.pull(&cx).await?,
                (true, false) => self.stop(&cx, AudioStopReason::HostDisabled).await?,
                _ => {}
            }
            let wake = now.saturating_add(PULL_US).saturating_mul(1000);
            sleep_until(Time::from_nanos(wake)).await;
        }
    }
    async fn start(&mut self, cx: &Cx) -> Result<(), Error> {
        let generation = AudioGeneration::from_raw(self.next_generation);
        self.next_generation += 1;
        let stream = self.profile.stream(generation);
        let capture = self.profile.capture(generation);
        let launch = Launch::new(
            &self.profile.image,
            &self.profile.display,
            None,
            Role::Audio,
            (self.profile.entropy)().map_err(|()| Error::Closed)?,
        )
        .and_then(Launch::retain_cleanup);
        let result = match launch {
            Ok((launch, retirement)) => {
                *self.profile.retired.lock().map_err(|_| Error::Poisoned)? = Some(retirement);
                self.unreaped = true;
                let deadline = Deadline::after(cx, START_TIMEOUT).map_err(media)?;
                Worker::start_audio(cx, launch, &capture, deadline).await
            }
            Err(error) => Err(error),
        };
        let Ok(worker) = result else {
            // Missing/unavailable monitor or a failed worker: typed and
            // terminal for this share, never a respawn loop.
            self.failed = true;
            return self
                .feed
                .with(|m| m.audio.fail(stream, AudioStopReason::HostDisabled));
        };
        self.worker = Some(worker);
        self.stream = Some(stream);
        self.feed
            .with(|m| m.audio.start(stream))?
            .map_err(|_| Error::Closed)
    }
    async fn pull(&mut self, cx: &Cx) -> Result<(), Error> {
        let (Some(worker), Some(stream)) = (self.worker.as_mut(), self.stream) else {
            return Ok(());
        };
        let capture = self.profile.capture(stream.generation);
        let deadline = Deadline::after(cx, PULL_TIMEOUT).map_err(media)?;
        let batch = worker
            .request_with_response_capacity(
                cx,
                Kind::ReadAudio,
                Vec::new(),
                Some(fr_media::worker::audio::MAX_BATCH_BYTES),
                deadline,
            )
            .await
            .map_err(|_| ())
            .and_then(|reply| decode_batch(reply.body(), &capture).map_err(|_| ()));
        let Ok(batch) = batch else {
            // The device changed or the worker failed: fence every lane first.
            self.failed = true;
            return self.stop(cx, AudioStopReason::DeviceChanged).await;
        };
        let owner = self.feed.owner()?;
        let pulled = owner.check().map_err(Error::Media)?.as_micros();
        let count = u64::try_from(batch.packets.len()).map_err(|_| Error::InvalidBudget)?;
        let frame_us = u64::from(FRAME_MS) * 1000;
        self.feed.with(|m| {
            for (index, unit) in (0_u64..).zip(batch.packets) {
                // The newest frame completed no later than this pull; each
                // older one a frame earlier. A late pull therefore ages its
                // backlog honestly and lanes drop it rather than burst it.
                let captured = pulled.saturating_sub((count - 1 - index) * frame_us);
                // Refusals are counted by the ring itself.
                let _ = m.audio.push(unit, captured);
            }
        })
    }
    async fn stop(&mut self, cx: &Cx, reason: AudioStopReason) -> Result<(), Error> {
        // Fence lanes BEFORE native cleanup: queued packets are discarded now.
        self.feed.with(|m| {
            if reason == AudioStopReason::HostDisabled && !self.failed {
                m.audio.idle();
            } else {
                m.audio.end(reason);
            }
        })?;
        self.stream = None;
        if let Some(mut worker) = self.worker.take() {
            if let Ok(deadline) = Deadline::after(cx, STOP_TIMEOUT) {
                if worker
                    .request(cx, Kind::Stop, Vec::new(), deadline)
                    .await
                    .is_err()
                {
                    worker.abort();
                }
                if let Ok(deadline) = Deadline::after(cx, STOP_TIMEOUT)
                    && worker.reap(cx, deadline).await.is_ok()
                {
                    self.unreaped = false;
                }
            } else {
                worker.abort();
            }
        }
        Ok(())
    }
}
impl Drop for AudioSource {
    fn drop(&mut self) {
        if let Ok(shared) = self.feed.members.upgrade().ok_or(()) {
            let mut members = shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            members.audio.end(AudioStopReason::SessionEnded);
        }
        if let Some(worker) = &mut self.worker {
            worker.abort();
        }
    }
}

fn media(error: worker::Error) -> Error {
    Error::Media(crate::media::Error::Worker(error))
}
