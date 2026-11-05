#![forbid(unsafe_code)]
//! One bound microphone uplink, isolated Opus decoder and actual virtual source.
//!
//! The containing session supplies controller authority, host approval and the
//! original local receipt of every record. This worker-only owner neither grants
//! those permissions nor advertises/attaches a network channel. A real `PipeWire`
//! consumer must request sound before packets enter the bounded jitter queue.
//! Bounded real PCM first prepares a complete native quantum. Afterwards one
//! exact-next real packet may decode ahead under its original receipt expiry.
//! After the first submission its output position is fixed; a later graph turn,
//! duplicate record or worker dequeue cannot move that position or renew expiry.
use super::{Demand, Error as DeviceError, Microphone, State, Statistics};
use crate::opus::{
    CodecLimits,
    process::{
        ProcessDecoder, Retirement,
        socket::{SocketDecoder, SocketFence},
    },
};
use fr_client::audio::{
    AudioJitterBuffer, JitterDrainResult,
    jitter::{JITTER_BUFFER_CAPACITY, JitterError},
    playout::{
        decoder::PolledDecoder,
        drift::{Drift, Span},
    },
};
use fr_core::audio::{AudioDirection, AudioStreamConfig, MAX_JITTER_CEILING_MS};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};
use fr_wire::{
    WireError,
    audio::{self, AudioConfiguration},
};
use std::{
    os::unix::net::UnixStream,
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    time::{Duration, Instant},
};

pub use crate::opus::playout::ReceiveResult;

mod startup;
use startup::{Frame as StartupFrame, Pcm as SensitivePcm, Startup};

const SETUP: Duration = Duration::from_secs(2);
const MAX_AGE: Duration = Duration::from_millis(MAX_JITTER_CEILING_MS as u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Binding,
    Closed,
    NotAcknowledged,
    DecoderNotReady,
    AlreadyAcknowledged,
    AcknowledgementUnknown,
    Expired,
    Clock,
    /// An outstanding decode missed its original native graph interval.
    MissedDemand,
    Metadata,
    Wire(WireError),
    Media(AudioMediaError),
    Jitter(JitterError),
    Device(DeviceError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "microphone-uplink: {self:?}")
    }
}
impl std::error::Error for Error {}
impl From<startup::Error> for Error {
    fn from(error: startup::Error) -> Self {
        match error {
            startup::Error::Configuration => Self::Configuration,
            startup::Error::Metadata => Self::Metadata,
            startup::Error::Capacity => Self::Media(AudioMediaError::BufferOverflow),
            startup::Error::Expired => Self::Expired,
            startup::Error::Timeline => Self::Clock,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderResult {
    Waiting,
    /// Accepted by the bounded native queue, not proof an application heard it.
    /// The first turn may admit a complete quantum's batch of real frames;
    /// `source_samples` is its first source timestamp and `output_samples` is
    /// the aggregate admitted span. Subsequent turns admit one frame each.
    Submitted {
        source_samples: u64,
        output_samples: u32,
        concealed: bool,
    },
}

/// Terminal control of the exact decoder transport. This is deliberately not
/// a reaping interface: an external socket owner cannot prove process exit.
pub trait DecoderStop: Clone {
    fn stop(&self);
}
impl DecoderStop for Retirement {
    fn stop(&self) {
        Retirement::stop(self);
    }
}
impl DecoderStop for SocketFence {
    fn stop(&self) {
        SocketFence::stop(self);
    }
}

/// The existing native playout state machine accepts an owned codec process or
/// a bounded socket to a codec owned by its containing process supervisor.
pub trait UplinkDecoder: PolledDecoder {
    type Stop: DecoderStop;
    fn stop_handle(&self) -> Self::Stop;
}
impl UplinkDecoder for ProcessDecoder {
    type Stop = Retirement;
    fn stop_handle(&self) -> Self::Stop {
        self.retirement()
    }
}
impl UplinkDecoder for SocketDecoder {
    type Stop = SocketFence;
    fn stop_handle(&self) -> Self::Stop {
        self.fence()
    }
}

/// Stops the exact source before requesting decoder retirement. This handle
/// cannot keep the native connection alive, reopen it, or stop its replacement.
#[derive(Clone)]
pub struct Control<S: DecoderStop = Retirement> {
    native: super::Control,
    decoder: S,
}
impl<S: DecoderStop> Control<S> {
    pub fn stop(&self) {
        self.native.stop();
        self.decoder.stop();
    }
    pub fn is_stopped(&self) -> bool {
        self.native.is_stopped()
    }
}

#[derive(Clone, Copy)]
struct Arrival {
    sequence: u64,
    expires: Instant,
}

struct Pending {
    // Only demand-bound PLC reserves a token before native PCM is ready. Real
    // packets may predecode; next_output is still immutable after first output.
    demand: Option<Demand>,
    span: Span,
    source: u64,
    expires: Instant,
    concealed: bool,
}

/// No mutable native/decoder escape. One original configuration, one bounded
/// jitter epoch and at most one child decode. All native access is confined to
/// the creating worker; the caller supplies ordinary readiness/timer scheduling.
pub struct Uplink<D: UplinkDecoder = ProcessDecoder> {
    binding: u32,
    offer: AudioConfiguration,
    config: AudioStreamConfig,
    device: Microphone,
    decoder: Option<D>,
    control: Control<D::Stop>,
    jitter: AudioJitterBuffer,
    arrivals: [Option<Arrival>; JITTER_BUFFER_CAPACITY],
    drift: Drift,
    pending: Option<Pending>,
    startup: Startup,
    next_output: Option<u64>,
    reorder_until: Option<Instant>,
    last_packet_expiry: Option<Instant>,
    origin: Instant,
    setup_until: Instant,
    configured: bool,
    acknowledged: bool,
    error: Option<Error>,
}
impl Uplink<ProcessDecoder> {
    /// `image` is the installed absolute codec image. Its existing supervisor
    /// owns spawning, bounded IPC and actual reaping.
    pub fn new(
        binding: u32,
        offer: AudioConfiguration,
        device: Microphone,
        image: &Path,
        permitted: impl FnMut() -> bool,
    ) -> Result<Self, Error> {
        Self::build(
            binding,
            offer,
            device,
            |limits| ProcessDecoder::new(image, limits).map(|(decoder, _)| decoder),
            permitted,
        )
    }
    /// Completion is separate from requested stop or dropping this owner.
    pub fn decoder_retirement(&self) -> Retirement {
        self.control.decoder.clone()
    }
}
impl Uplink<SocketDecoder> {
    /// The containing supervisor owns the actual codec child and its exit
    /// receipt. This socket must be its original private connected IPC endpoint;
    /// the device worker never launches or claims to reap a descendant.
    pub fn from_socket(
        binding: u32,
        offer: AudioConfiguration,
        device: Microphone,
        socket: UnixStream,
        permitted: impl FnMut() -> bool,
    ) -> Result<Self, Error> {
        Self::build(
            binding,
            offer,
            device,
            |limits| SocketDecoder::new(socket, limits),
            permitted,
        )
    }
}
impl<D: UplinkDecoder> Uplink<D> {
    fn build(
        binding: u32,
        offer: AudioConfiguration,
        device: Microphone,
        make_decoder: impl FnOnce(CodecLimits) -> Result<D, AudioMediaError>,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Self, Error> {
        let origin = Instant::now();
        let setup_until = origin.checked_add(SETUP).ok_or(Error::Clock)?;
        if binding == 0 {
            return Err(Error::Binding);
        }
        offer.validate().map_err(Error::Wire)?;
        let config = AudioStreamConfig::new(
            offer.direction,
            offer.generation,
            offer.channels,
            offer.frame_duration_ms,
            offer.jitter_target_ms,
        )
        .map_err(|_| Error::Configuration)?;
        if config.direction() != AudioDirection::Uplink
            || !matches!(config.frame_duration_ms(), 10 | 20)
            || config != device.configuration()
            || config.expected_samples_per_frame() > offer.max_decoded_samples
        {
            return Err(Error::Configuration);
        }
        let limits = CodecLimits::new(
            usize::try_from(offer.max_packet_bytes).map_err(|_| Error::Configuration)?,
            offer.max_decoded_samples,
        )
        .map_err(Error::Media)?;
        let jitter = AudioJitterBuffer::with_config(config).map_err(Error::Jitter)?;
        let startup = Startup::new(config)?;
        permission(&mut permitted)?;
        let mut decoder = make_decoder(limits).map_err(Error::Media)?;
        decoder.configure(config).map_err(Error::Media)?;
        let control = Control {
            native: device.control(),
            decoder: decoder.stop_handle(),
        };
        let mut owner = Self {
            binding,
            offer,
            config,
            device,
            decoder: Some(decoder),
            control,
            jitter,
            arrivals: [None; JITTER_BUFFER_CAPACITY],
            drift: Drift::default(),
            pending: None,
            startup,
            next_output: None,
            reorder_until: None,
            last_packet_expiry: None,
            origin,
            setup_until,
            configured: false,
            acknowledged: false,
            error: None,
        };
        owner.operation(|this| {
            this.device
                .poll_demand(&mut permitted)
                .map_err(Error::Device)?;
            this.setup_live()?;
            Ok(())
        })?;
        Ok(owner)
    }

    pub fn control(&self) -> Control<D::Stop> {
        self.control.clone()
    }
    pub const fn configuration(&self) -> AudioConfiguration {
        self.offer
    }
    pub const fn state(&self) -> State {
        self.device.state()
    }
    pub const fn error(&self) -> Option<Error> {
        self.error
    }
    pub const fn queued_packets(&self) -> usize {
        self.jitter.queued_packet_count()
    }
    pub fn statistics(&self) -> Statistics {
        self.device.statistics()
    }

    pub fn poll_ready(&mut self, mut permitted: impl FnMut() -> bool) -> Result<bool, Error> {
        self.check()?;
        self.operation(|this| {
            this.device
                .poll_demand(&mut permitted)
                .map_err(Error::Device)?;
            this.setup_live()?;
            this.poll_decoder()?;
            Ok(this.configured && this.device.state() == State::Ready)
        })
    }

    /// The authenticated channel must admit this exact record before returning
    /// success. Failed/unknown admission retires the source and child; there is
    /// no retry that could accidentally re-enable an earlier audio generation.
    pub fn acknowledge(
        &mut self,
        mut permitted: impl FnMut() -> bool,
        send: impl FnOnce(&[u8]) -> Result<(), ()>,
    ) -> Result<(), Error> {
        self.check()?;
        if self.acknowledged {
            return Err(Error::AlreadyAcknowledged);
        }
        self.operation(|this| {
            this.device
                .poll_demand(&mut permitted)
                .map_err(Error::Device)?;
            this.setup_live()?;
            this.poll_decoder()?;
            if !this.configured || this.device.state() != State::Ready {
                return Err(Error::DecoderNotReady);
            }
            let mut record = [0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
            audio::encode_configured(
                &audio::AudioConfigured {
                    direction: this.config.direction(),
                    generation: this.config.generation(),
                    accepted: true,
                    actual_channels: this.config.channels(),
                    actual_sample_rate: this.config.sample_rate(),
                    actual_frame_duration_ms: this.config.frame_duration_ms(),
                },
                this.binding,
                &mut record,
            )
            .map_err(Error::Wire)?;
            catch_unwind(AssertUnwindSafe(|| send(&record)))
                .map_err(|_| Error::AcknowledgementUnknown)?
                .map_err(|()| Error::AcknowledgementUnknown)?;
            this.device
                .poll_demand(&mut permitted)
                .map_err(Error::Device)?;
            this.setup_live()?;
            this.acknowledged = true;
            Ok(())
        })
    }

    /// `arrived` is the ORIGINAL LOCAL receipt, before any worker inbox. Future
    /// times are refused. Expired arrivals are ignored without renewing startup,
    /// concealment or retained-packet deadlines. Matching stop fences first,
    /// before native polling or calling any user-supplied permission predicate.
    pub fn receive_record_at(
        &mut self,
        bytes: &[u8],
        arrived: Instant,
        permitted: impl FnMut() -> bool,
    ) -> Result<ReceiveResult, Error> {
        self.receive(bytes, arrived, None, permitted)
    }

    /// Preserve an earlier original transport expiry through jitter, decoder
    /// custody and native PCM. Neither this worker dequeue nor a later authority
    /// renewal may extend it; the ordinary 100-ms ceiling still applies.
    pub fn receive_record_until(
        &mut self,
        bytes: &[u8],
        arrived: Instant,
        expires: Instant,
        permitted: impl FnMut() -> bool,
    ) -> Result<ReceiveResult, Error> {
        self.receive(bytes, arrived, Some(expires), permitted)
    }

    fn receive(
        &mut self,
        bytes: &[u8],
        arrived: Instant,
        earlier_expiry: Option<Instant>,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<ReceiveResult, Error> {
        if let Ok(stop) = audio::decode_stop(bytes, self.binding)
            && stop.direction == self.offer.direction
            && stop.generation == self.offer.generation
        {
            self.stop();
            return Ok(ReceiveResult::Stopped(stop.reason));
        }
        self.check()?;
        self.operation(|this| {
            let packet = match audio::decode_packet(bytes, this.binding) {
                Ok(packet) => packet,
                Err(WireError::UnsupportedKind) => {
                    audio::decode_stop(bytes, this.binding).map_err(Error::Wire)?;
                    return Ok(ReceiveResult::Ignored);
                }
                Err(error) => return Err(Error::Wire(error)),
            };
            if packet.direction != this.offer.direction
                || packet.generation != this.offer.generation
            {
                return Ok(ReceiveResult::Ignored);
            }
            if !this.acknowledged {
                return Err(Error::NotAcknowledged);
            }
            if packet.payload.len()
                > usize::try_from(this.offer.max_packet_bytes).map_err(|_| Error::Configuration)?
                || u32::from(packet.duration_samples) > this.offer.max_decoded_samples
                || u32::from(packet.duration_samples) != this.config.expected_samples_per_frame()
            {
                return Err(Error::Wire(WireError::ResourceLimit));
            }
            let now = Instant::now();
            if arrived > now {
                return Err(Error::Clock);
            }
            let ceiling = arrived.checked_add(MAX_AGE).ok_or(Error::Clock)?;
            let expires = earlier_expiry.map_or(ceiling, |expiry| expiry.min(ceiling));
            if expires <= arrived {
                return Err(Error::Clock);
            }
            if now >= expires {
                return Ok(ReceiveResult::Ignored);
            }
            let Some(demand) = this
                .device
                .poll_demand(&mut permitted)
                .map_err(Error::Device)?
            else {
                // A published source without a consumer is not a running clock.
                return Ok(ReceiveResult::Ignored);
            };
            this.retained_live()?;
            if Instant::now() >= expires {
                return Ok(ReceiveResult::Ignored);
            }
            let unit = AudioAccessUnit::new(
                packet.direction,
                packet.generation,
                packet.sequence,
                packet.timestamp_samples,
                packet.duration_samples,
                false,
                packet.payload,
            )
            .map_err(Error::Media)?;
            if !this.jitter.try_push_packet(unit).map_err(Error::Jitter)? {
                return Ok(ReceiveResult::Ignored);
            }
            // Startup eviction removes the corresponding metadata; duplicates
            // never get here and therefore cannot replace an original expiry.
            for entry in &mut this.arrivals {
                if entry.is_some_and(|a| !this.jitter.contains_sequence(a.sequence)) {
                    *entry = None;
                }
            }
            let slot = this
                .arrivals
                .iter_mut()
                .find(|a| a.is_none())
                .ok_or(Error::Metadata)?;
            *slot = Some(Arrival {
                sequence: packet.sequence,
                expires,
            });
            if this.reorder_until.is_none() {
                this.reorder_until = Some(
                    arrived
                        .checked_add(Duration::from_millis(u64::from(
                            this.offer.jitter_target_ms,
                        )))
                        .ok_or(Error::Clock)?,
                );
            }
            this.observe(packet.timestamp_samples, demand)?;
            Ok(ReceiveResult::Queued)
        })
    }

    /// One bounded worker turn: poll native demand and at most one codec result
    /// or admission, except the initial batch of at most three real PCM frames.
    /// One exact-next real packet may predecode while an earlier native frame
    /// still spans graph quanta; no PLC is generated ahead. Output retains its
    /// fixed position, and a missed interval cannot be rebased.
    pub fn service(&mut self, mut permitted: impl FnMut() -> bool) -> Result<RenderResult, Error> {
        self.check()?;
        self.operation(|this| {
            let demand = this
                .device
                .poll_demand(&mut permitted)
                .map_err(Error::Device)?;
            this.poll_decoder()?;
            if !this.acknowledged {
                this.setup_live()?;
                return Ok(RenderResult::Waiting);
            }
            this.retained_live()?;
            let Some(demand) = demand else {
                return Ok(RenderResult::Waiting);
            };
            if this.next_output.is_none() {
                return this.service_startup(demand, &mut permitted);
            }
            let start = this.next_output.unwrap_or(demand.output_start());
            if start < demand.output_start() {
                return Err(Error::MissedDemand);
            }
            if let Some(pending) = this.pending.as_ref() {
                if pending.demand.is_some_and(|original| {
                    original.serial() != demand.serial() || Instant::now() >= original.valid_until()
                }) {
                    return Err(Error::MissedDemand);
                }
                if start >= demand.output_end() || demand.available_blocks() == 0 {
                    return Ok(RenderResult::Waiting);
                }
                let Some(pcm) = this.decoder()?.poll_pcm().map_err(Error::Media)? else {
                    return Ok(RenderResult::Waiting);
                };
                let mut pcm = SensitivePcm(pcm);
                let pending = this.pending.take().ok_or(Error::Metadata)?;
                return this.submit(&mut pcm, &pending, demand, start, &mut permitted);
            }
            if this
                .reorder_until
                .is_none_or(|until| Instant::now() < until)
                || demand.available_blocks() == 0
            {
                return Ok(RenderResult::Waiting);
            }
            if start >= demand.output_end() {
                if let Some(packet) = this.jitter.predecode_packet() {
                    this.decode_packet(&packet)?;
                }
                return Ok(RenderResult::Waiting);
            }
            this.decode(demand)
        })
    }

    fn service_startup(
        &mut self,
        demand: Demand,
        permitted: &mut impl FnMut() -> bool,
    ) -> Result<RenderResult, Error> {
        if !self.startup.needs_frame(demand.frames())? {
            if self.pending.is_some() {
                return Err(Error::Metadata);
            }
            let Some(batch) =
                self.startup
                    .take(demand.frames(), demand.available_blocks(), Instant::now())?
            else {
                return Ok(RenderResult::Waiting);
            };
            // No native poll or decoder work can run between these bounded
            // submissions. The owner-thread callback sees a complete first
            // quantum; any error returns through operation(), fencing and
            // clearing the partial batch before another callback can run.
            let submitted =
                batch.admit::<Error>(demand.output_start(), Instant::now, |frame, start| {
                    let pending = Pending {
                        demand: None,
                        span: frame.span,
                        source: frame.pcm.0.timestamp_samples(),
                        expires: frame.expires,
                        concealed: false,
                    };
                    self.submit(&mut frame.pcm, &pending, demand, start, permitted)
                        .map(|_| ())
                })?;
            return Ok(RenderResult::Submitted {
                source_samples: submitted.source_samples,
                output_samples: submitted.output_samples,
                concealed: false,
            });
        }
        if let Some(pending) = self.pending.as_ref() {
            if pending.demand.is_some() || pending.concealed {
                return Err(Error::Metadata);
            }
            let Some(pcm) = self.decoder()?.poll_pcm().map_err(Error::Media)? else {
                return Ok(RenderResult::Waiting);
            };
            let pcm = SensitivePcm(pcm);
            let pending = self.pending.take().ok_or(Error::Metadata)?;
            self.validate_pcm(&pcm.0, &pending)?;
            self.startup.push(
                StartupFrame {
                    pcm,
                    span: pending.span,
                    expires: pending.expires,
                },
                demand.frames(),
                Instant::now(),
            )?;
            // Select an absolute output start only on the next service turn,
            // after polling a fresh demand with a complete batch already ready.
            return Ok(RenderResult::Waiting);
        }
        if self
            .reorder_until
            .is_none_or(|until| Instant::now() < until)
            || demand.available_blocks() == 0
        {
            return Ok(RenderResult::Waiting);
        }
        let packet = if self.startup.is_empty() {
            match self.jitter.drain_for_playout().map_err(Error::Jitter)? {
                JitterDrainResult::Packet(packet) => Some(packet),
                JitterDrainResult::Underrun => None,
                JitterDrainResult::Plc { .. } => return Err(Error::Metadata),
            }
        } else {
            // An absent exact-next packet leaves a bounded gap until its
            // original receipt expires. Startup must never invent early PLC.
            self.jitter.predecode_packet()
        };
        if let Some(packet) = packet {
            self.decode_packet(&packet)?;
        }
        Ok(RenderResult::Waiting)
    }

    fn decode(&mut self, demand: Demand) -> Result<RenderResult, Error> {
        let source_next = self.jitter.next_sample_timestamp();
        let (source, expires, concealed) =
            match self.jitter.drain_for_playout().map_err(Error::Jitter)? {
                JitterDrainResult::Packet(packet) => {
                    self.decode_packet(&packet)?;
                    return Ok(RenderResult::Waiting);
                }
                JitterDrainResult::Plc {
                    duration_samples, ..
                } => {
                    let source = source_next.ok_or(Error::Metadata)?;
                    let expires = self.last_packet_expiry.ok_or(Error::Metadata)?;
                    if Instant::now() >= expires {
                        return Err(Error::Expired);
                    }
                    // ProcessDecoder always returns pending; do not silently discard
                    // unexpected synchronous output if that contract ever changes.
                    if let Some(pcm) = self
                        .decoder()?
                        .submit_plc(duration_samples)
                        .map_err(Error::Media)?
                    {
                        drop(SensitivePcm(pcm));
                        return Err(Error::Metadata);
                    }
                    (source, expires, true)
                }
                JitterDrainResult::Underrun => return Ok(RenderResult::Waiting),
            };
        self.pending = Some(Pending {
            demand: Some(demand),
            span: self.drift.plan(self.config.expected_samples_per_frame()),
            source,
            expires,
            concealed,
        });
        Ok(RenderResult::Waiting)
    }

    fn decode_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), Error> {
        let entry = self
            .arrivals
            .iter_mut()
            .find(|a| a.is_some_and(|a| a.sequence == packet.sequence()))
            .and_then(Option::take)
            .ok_or(Error::Metadata)?;
        if Instant::now() >= entry.expires {
            return Err(Error::Expired);
        }
        self.decoder()?
            .submit_packet(packet)
            .map_err(Error::Media)?;
        self.last_packet_expiry = Some(entry.expires);
        self.pending = Some(Pending {
            demand: None,
            span: self.drift.plan(self.config.expected_samples_per_frame()),
            source: packet.timestamp_samples(),
            expires: entry.expires,
            concealed: false,
        });
        Ok(())
    }

    fn validate_pcm(&self, pcm: &AudioPcmFrame, pending: &Pending) -> Result<(), Error> {
        let nominal = self.config.expected_samples_per_frame();
        if pcm.generation() != self.config.generation()
            || pcm.channels() != self.config.channels()
            || pcm.sample_rate() != self.config.sample_rate()
            || pcm.samples_per_channel()
                != usize::try_from(nominal).map_err(|_| Error::Configuration)?
            || pcm.timestamp_samples() != pending.source
        {
            return Err(Error::Metadata);
        }
        if Instant::now() >= pending.expires {
            return Err(Error::Expired);
        }
        Ok(())
    }

    fn submit(
        &mut self,
        pcm: &mut SensitivePcm,
        pending: &Pending,
        demand: Demand,
        start: u64,
        permitted: &mut impl FnMut() -> bool,
    ) -> Result<RenderResult, Error> {
        self.validate_pcm(&pcm.0, pending)?;
        let nominal = self.config.expected_samples_per_frame();
        let output_samples = pending.span.samples();
        let end = start
            .checked_add(u64::from(output_samples))
            .ok_or(Error::Clock)?;
        let source_end = pending
            .source
            .checked_add(u64::from(nominal))
            .ok_or(Error::Clock)?;
        pcm.0
            .resample_for_clock(usize::try_from(output_samples).map_err(|_| Error::Configuration)?)
            .map_err(Error::Media)?;
        self.device
            .submit_for_demand(&pcm.0, demand, start, pending.expires, permitted)
            .map_err(Error::Device)?;
        self.drift.submitted(pending.span, source_end, end);
        self.next_output = Some(end);
        Ok(RenderResult::Submitted {
            source_samples: pending.source,
            output_samples,
            concealed: pending.concealed,
        })
    }

    fn observe(&mut self, source: u64, demand: Demand) -> Result<(), Error> {
        // The fixed callback/handoff allowance belongs in the target. Its distance
        // from the extrapolated graph cursor shrinks within one turn and must
        // not turn callback phase into a moving jitter target.
        let target = (u64::from(self.offer.jitter_target_ms) * 48)
            .checked_add(u64::from(demand.lead_frames()))
            .ok_or(Error::Clock)?;
        self.drift.observe(
            source,
            demand.graph_samples(),
            u64::try_from(self.origin.elapsed().as_micros()).map_err(|_| Error::Clock)?,
            target,
        );
        Ok(())
    }

    fn decoder(&mut self) -> Result<&mut D, Error> {
        self.decoder.as_mut().ok_or(Error::Closed)
    }
    fn poll_decoder(&mut self) -> Result<(), Error> {
        if !self.configured {
            self.configured = self.decoder()?.poll_configured().map_err(Error::Media)?;
        }
        Ok(())
    }
    fn setup_live(&self) -> Result<(), Error> {
        if Instant::now() >= self.setup_until {
            Err(Error::Expired)
        } else {
            Ok(())
        }
    }
    fn retained_live(&mut self) -> Result<(), Error> {
        let now = Instant::now();
        self.startup.check_live(now)?;
        if self.arrivals.iter().flatten().any(|a| now >= a.expires)
            || self.pending.as_ref().is_some_and(|p| now >= p.expires)
        {
            Err(Error::Expired)
        } else {
            Ok(())
        }
    }
    fn check(&mut self) -> Result<(), Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.control.is_stopped() {
            self.stop();
            return Err(Error::Closed);
        }
        Ok(())
    }
    pub fn stop(&mut self) {
        // Fence source first, then erase retained work and request decoder stop,
        // before native destruction can enter foreign code. No reaping claim.
        self.control.native.stop();
        self.jitter.stop();
        self.pending = None;
        self.startup.clear();
        self.arrivals.fill(None);
        self.control.decoder.stop();
        self.decoder = None;
        self.device.close();
        self.error.get_or_insert(Error::Closed);
    }
    fn operation<T>(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut guard = Guard {
            owner: self,
            finished: false,
        };
        let result = action(guard.owner);
        if let Err(error) = result {
            guard.owner.error = Some(error);
            guard.owner.stop();
        }
        guard.finished = true;
        result
    }
}
impl<D: UplinkDecoder> Drop for Uplink<D> {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Guard<'a, D: UplinkDecoder> {
    owner: &'a mut Uplink<D>,
    finished: bool,
}
impl<D: UplinkDecoder> Drop for Guard<'_, D> {
    fn drop(&mut self) {
        if !self.finished {
            self.owner.stop();
        }
    }
}
fn permission(permitted: &mut impl FnMut() -> bool) -> Result<(), Error> {
    if catch_unwind(AssertUnwindSafe(permitted)).unwrap_or(false) {
        Ok(())
    } else {
        Err(Error::Device(DeviceError::PermissionEnded))
    }
}
