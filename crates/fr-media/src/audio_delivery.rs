//! Host playback-audio fan-out (plan §15.4; PROTOCOL.md 0x0060-0x0063).
//!
//! One OS share owns at most one live audio source. Its encoded packets enter
//! ONE bounded ring (count AND bytes); each admitted viewer that selected the
//! audio-down capability owns an [`AudioLane`] cursor into it. There is no
//! per-viewer packet FIFO: a slow viewer skips evicted or obsolete packets and
//! counts them, it never receives a catch-up burst of old sound.
//!
//! A lane sends `AudioConfiguration`, then NOTHING until the viewer's matching
//! `AudioConfigured` arrives, then only packets captured after that
//! acknowledgement. `AudioStop` fences the lane before any queued packet can
//! be sent. Audio is never capture/source freshness evidence: nothing here
//! touches frame identity, source progress or view readiness.
//!
//! Pure state: the caller owns transport sends, authority checks and clocks.
use crate::audio::AudioAccessUnit;
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, AudioStopReason, MAX_OPUS_PAYLOAD_BYTES,
    OPUS_SAMPLE_RATE,
};
use fr_wire::audio::{AudioConfiguration, AudioConfigured, AudioStop};
use std::collections::VecDeque;

/// Ring bound in packets: 160 ms of 20 ms frames.
pub const RING_PACKETS: usize = 8;
/// Ring bound in Opus payload bytes, independent of the count bound.
pub const RING_BYTES: usize = 4 * 1024;
/// A packet older than this since its capture is obsolete for every lane:
/// the receiver's playout expires queued audio 100 ms after ARRIVAL, so a
/// late backlog is dropped here (and counted) instead of arriving as a burst.
pub const MAX_PACKET_AGE_US: u64 = 40_000;
/// A viewer must acknowledge a configuration within this window, which covers
/// its local device startup (up to 2 s) plus network round trips.
pub const CONFIGURE_TIMEOUT_US: u64 = 5_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// A generation that is not strictly newer, or a record naming another one.
    StaleGeneration,
    /// A record the current lane/source state does not admit.
    WrongState,
    /// Invalid profile, packet shape or ordering.
    Invalid,
}

/// The negotiated stream a live source advertises in `AudioConfiguration`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceStream {
    pub generation: AudioGeneration,
    pub channels: AudioChannels,
    pub frame_duration_ms: u16,
    pub max_packet_bytes: u16,
    pub jitter_target_ms: u16,
}
impl SourceStream {
    pub fn configuration(self) -> AudioConfiguration {
        AudioConfiguration {
            direction: AudioDirection::Downlink,
            generation: self.generation,
            channels: self.channels,
            sample_rate: OPUS_SAMPLE_RATE,
            frame_duration_ms: self.frame_duration_ms,
            max_packet_bytes: u32::from(self.max_packet_bytes),
            max_decoded_samples: self.frame_samples(),
            jitter_target_ms: self.jitter_target_ms,
        }
    }
    const fn frame_samples(self) -> u32 {
        OPUS_SAMPLE_RATE / 1000 * self.frame_duration_ms as u32
    }
    fn validate(self) -> Result<(), Error> {
        if self.generation.as_raw() == 0
            || !matches!(self.frame_duration_ms, 10 | 20)
            || self.max_packet_bytes == 0
            || usize::from(self.max_packet_bytes) > MAX_OPUS_PAYLOAD_BYTES
            || self.configuration().validate().is_err()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceState {
    /// No source is running (no demand yet, or between demand periods).
    Idle,
    Live(SourceStream),
    /// The source ended for a typed reason; lanes on it send `AudioStop`.
    Ended {
        generation: AudioGeneration,
        reason: AudioStopReason,
    },
}

/// One captured packet and its host-monotonic capture completion time.
#[derive(Debug, Clone, Copy)]
pub struct Stamped {
    pub unit: AudioAccessUnit,
    pub captured_us: u64,
}

/// Content-free source accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingCounters {
    pub pushed: u64,
    /// Oldest packets displaced by the count or byte bound.
    pub evicted: u64,
    /// Packets refused for a wrong generation, shape or ordering.
    pub refused: u64,
}

/// The single bounded packet ring of one share's audio source.
#[derive(Debug)]
pub struct AudioRing {
    state: SourceState,
    packets: VecDeque<Stamped>,
    bytes: usize,
    last_generation: Option<AudioGeneration>,
    last_sequence: Option<u64>,
    counters: RingCounters,
}
impl Default for AudioRing {
    fn default() -> Self {
        Self::new()
    }
}
impl AudioRing {
    pub fn new() -> Self {
        Self {
            state: SourceState::Idle,
            packets: VecDeque::with_capacity(RING_PACKETS),
            bytes: 0,
            last_generation: None,
            last_sequence: None,
            counters: RingCounters::default(),
        }
    }
    pub const fn state(&self) -> SourceState {
        self.state
    }
    pub const fn counters(&self) -> RingCounters {
        self.counters
    }
    pub fn len(&self) -> usize {
        self.packets.len()
    }
    pub fn is_empty(&self) -> bool {
        self.packets.is_empty()
    }
    pub const fn bytes(&self) -> usize {
        self.bytes
    }
    /// Start a new source generation. Generations never repeat or go back,
    /// so a restarted source cannot relabel an old stream's packets.
    pub fn start(&mut self, stream: SourceStream) -> Result<(), Error> {
        stream.validate()?;
        if matches!(self.state, SourceState::Live(_))
            || self
                .last_generation
                .is_some_and(|g| stream.generation.as_raw() <= g.as_raw())
        {
            return Err(Error::StaleGeneration);
        }
        self.clear();
        self.last_generation = Some(stream.generation);
        self.last_sequence = None;
        self.state = SourceState::Live(stream);
        Ok(())
    }
    /// End the live source: every queued packet is discarded immediately.
    pub fn end(&mut self, reason: AudioStopReason) {
        if let SourceState::Live(stream) = self.state {
            self.state = SourceState::Ended {
                generation: stream.generation,
                reason,
            };
        }
        self.clear();
    }
    /// A source generation that never became live (missing monitor, failed
    /// worker): consume the generation and publish the typed end so every
    /// waiting lane learns why instead of waiting silently.
    pub fn fail(&mut self, stream: SourceStream, reason: AudioStopReason) {
        if matches!(self.state, SourceState::Live(_))
            || self
                .last_generation
                .is_some_and(|g| stream.generation.as_raw() <= g.as_raw())
        {
            return;
        }
        self.clear();
        self.last_generation = Some(stream.generation);
        self.state = SourceState::Ended {
            generation: stream.generation,
            reason,
        };
    }
    /// No demand remains: return to idle (the next start needs a new generation).
    pub fn idle(&mut self) {
        self.state = SourceState::Idle;
        self.clear();
    }
    fn clear(&mut self) {
        self.packets.clear();
        self.bytes = 0;
    }
    /// Admit one encoded packet of the live generation. Out-of-order,
    /// foreign-generation or oversized packets are refused and counted.
    pub fn push(&mut self, unit: AudioAccessUnit, captured_us: u64) -> Result<(), Error> {
        let SourceState::Live(stream) = self.state else {
            self.counters.refused += 1;
            return Err(Error::WrongState);
        };
        if unit.generation() != stream.generation {
            self.counters.refused += 1;
            return Err(Error::StaleGeneration);
        }
        if unit.direction() != AudioDirection::Downlink
            || u32::from(unit.duration_samples()) != stream.frame_samples()
            || unit.payload().len() > usize::from(stream.max_packet_bytes)
            || self.last_sequence.is_some_and(|s| unit.sequence() <= s)
        {
            self.counters.refused += 1;
            return Err(Error::Invalid);
        }
        let size = unit.payload().len();
        while self.packets.len() >= RING_PACKETS || self.bytes + size > RING_BYTES {
            let Some(old) = self.packets.pop_front() else {
                break;
            };
            self.bytes -= old.unit.payload().len();
            self.counters.evicted += 1;
        }
        self.last_sequence = Some(unit.sequence());
        self.bytes += size;
        self.packets.push_back(Stamped { unit, captured_us });
        self.counters.pushed += 1;
        Ok(())
    }
    pub fn get(&self, sequence: u64) -> Option<&Stamped> {
        let first = self.packets.front()?.unit.sequence();
        let index = usize::try_from(sequence.checked_sub(first)?).ok()?;
        self.packets
            .get(index)
            .filter(|p| p.unit.sequence() == sequence)
    }
    fn oldest(&self) -> Option<u64> {
        self.packets.front().map(|p| p.unit.sequence())
    }
    fn newest(&self) -> Option<u64> {
        self.last_sequence
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LaneState {
    /// Attached, nothing sent yet (source idle or not yet live).
    Waiting,
    /// `AudioConfiguration` sent (or about to be); no packet may follow yet.
    Configuring {
        stream: SourceStream,
        epoch: AudioGeneration,
        until_us: Option<u64>,
    },
    /// Acknowledged. `next` is the next ring sequence to consider.
    Active {
        stream: SourceStream,
        epoch: AudioGeneration,
        next: u64,
    },
    /// An `AudioStop` must be sent before anything else.
    Stopping {
        epoch: AudioGeneration,
        reason: AudioStopReason,
        then: Then,
    },
    /// Terminal for this lane (viewer refusal/stop, timeout, source end).
    Stopped,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    /// Reconfigure once the source is live again (a restarted source).
    Reconfigure,
    Stop,
}

/// What the owner should send next on this viewer's audio routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneAction {
    Nothing,
    /// Reliable `AudioConfiguration` on the audio-down stream.
    Configure(AudioConfiguration),
    /// One `AudioPacket` datagram: the ring packet with this sequence, sent
    /// under this lane's own audio generation.
    Packet {
        sequence: u64,
        generation: AudioGeneration,
    },
    /// Reliable `AudioStop` on the audio-down stream.
    Stop(AudioStop),
}

/// Content-free per-viewer accounting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaneCounters {
    pub configurations: u64,
    pub sent: u64,
    /// Captured before this viewer's acknowledgement: never sent.
    pub skipped_before_ack: u64,
    /// Older than [`MAX_PACKET_AGE_US`] when the lane reached them.
    pub dropped_obsolete: u64,
    /// Evicted from the bounded ring before this lane sent them.
    pub dropped_evicted: u64,
    /// Viewer records naming a stale or unknown generation.
    pub stale_refused: u64,
    pub stops: u64,
    /// Fresh generations after the viewer's own output reset.
    pub restarts: u64,
}

/// Fresh generations one viewer may request after its local output reset
/// (`AudioStop` with `DeviceChanged`). Beyond this the lane ends, typed.
pub const MAX_LANE_RESTARTS: u64 = 8;

/// One viewer's cursor into the shared ring. The wire generation is this
/// lane's OWN epoch: strictly increasing per viewer, never reused, and
/// independent of the private source generation. A viewer reset or a
/// restarted source therefore always gets a new epoch, and a stale record
/// from an older epoch can never address the current stream.
#[derive(Debug)]
pub struct AudioLane {
    state: LaneState,
    last_epoch: Option<AudioGeneration>,
    counters: LaneCounters,
}
impl Default for AudioLane {
    fn default() -> Self {
        Self::new()
    }
}
impl AudioLane {
    pub const fn new() -> Self {
        Self {
            state: LaneState::Waiting,
            last_epoch: None,
            counters: LaneCounters {
                configurations: 0,
                sent: 0,
                skipped_before_ack: 0,
                dropped_obsolete: 0,
                dropped_evicted: 0,
                stale_refused: 0,
                stops: 0,
                restarts: 0,
            },
        }
    }
    pub const fn counters(&self) -> LaneCounters {
        self.counters
    }
    pub const fn is_active(&self) -> bool {
        matches!(self.state, LaneState::Active { .. })
    }
    pub const fn is_stopped(&self) -> bool {
        matches!(self.state, LaneState::Stopped)
    }
    /// The lane still wants the source (not terminal).
    pub const fn wants_audio(&self) -> bool {
        !matches!(
            self.state,
            LaneState::Stopped
                | LaneState::Stopping {
                    then: Then::Stop,
                    ..
                }
        )
    }
    /// The next strictly newer lane epoch; `None` when exhausted.
    fn next_epoch(&mut self) -> Option<AudioGeneration> {
        let next = self
            .last_epoch
            .map_or(Some(1), |g| g.as_raw().checked_add(1))?;
        let epoch = AudioGeneration::from_raw(next);
        self.last_epoch = Some(epoch);
        Some(epoch)
    }
    /// The next action. Obsolete and evicted packets are skipped and counted
    /// here, so the caller never sends one; a lane never jumps backwards.
    pub fn next(&mut self, ring: &AudioRing, now_us: u64) -> LaneAction {
        loop {
            match self.state {
                LaneState::Stopped => return LaneAction::Nothing,
                LaneState::Stopping { epoch, reason, .. } => {
                    return LaneAction::Stop(AudioStop {
                        direction: AudioDirection::Downlink,
                        generation: epoch,
                        reason,
                    });
                }
                LaneState::Waiting => match ring.state() {
                    SourceState::Live(stream) => {
                        self.state = self.next_epoch().map_or(LaneState::Stopped, |epoch| {
                            LaneState::Configuring {
                                stream,
                                epoch,
                                until_us: None,
                            }
                        });
                    }
                    SourceState::Ended { reason, .. } => {
                        // Typed absence: this viewer never got a configuration,
                        // but it learns that host audio ended and why.
                        self.state = self.next_epoch().map_or(LaneState::Stopped, |epoch| {
                            LaneState::Stopping {
                                epoch,
                                reason,
                                then: Then::Stop,
                            }
                        });
                    }
                    SourceState::Idle => return LaneAction::Nothing,
                },
                LaneState::Configuring {
                    stream,
                    epoch,
                    until_us,
                } => {
                    if self.superseded(ring, stream, epoch) {
                        continue;
                    }
                    match until_us {
                        None => {
                            return LaneAction::Configure(AudioConfiguration {
                                generation: epoch,
                                ..stream.configuration()
                            });
                        }
                        Some(until) if now_us >= until => {
                            self.state = LaneState::Stopping {
                                epoch,
                                reason: AudioStopReason::SessionEnded,
                                then: Then::Stop,
                            };
                        }
                        Some(_) => return LaneAction::Nothing,
                    }
                }
                LaneState::Active {
                    stream,
                    epoch,
                    next,
                } => {
                    if self.superseded(ring, stream, epoch) {
                        continue;
                    }
                    if let Some(oldest) = ring.oldest()
                        && next < oldest
                    {
                        self.counters.dropped_evicted += oldest - next;
                        self.state = LaneState::Active {
                            stream,
                            epoch,
                            next: oldest,
                        };
                        continue;
                    }
                    let Some(packet) = ring.get(next) else {
                        return LaneAction::Nothing;
                    };
                    if now_us.saturating_sub(packet.captured_us) > MAX_PACKET_AGE_US {
                        self.counters.dropped_obsolete += 1;
                        self.state = LaneState::Active {
                            stream,
                            epoch,
                            next: next + 1,
                        };
                        continue;
                    }
                    return LaneAction::Packet {
                        sequence: next,
                        generation: epoch,
                    };
                }
            }
        }
    }
    /// A source that ended or restarted under this lane: stop first.
    fn superseded(
        &mut self,
        ring: &AudioRing,
        stream: SourceStream,
        epoch: AudioGeneration,
    ) -> bool {
        let (reason, then) = match ring.state() {
            SourceState::Live(live) if live.generation == stream.generation => return false,
            SourceState::Live(_) => (AudioStopReason::DeviceChanged, Then::Reconfigure),
            SourceState::Ended { reason, .. } => (reason, Then::Stop),
            SourceState::Idle => (AudioStopReason::HostDisabled, Then::Reconfigure),
        };
        self.state = LaneState::Stopping {
            epoch,
            reason,
            then,
        };
        true
    }
    /// The configuration record was accepted by the transport. The ORIGINAL
    /// acknowledgement deadline starts now and is never renewed.
    pub fn configuration_sent(&mut self, now_us: u64) -> Result<(), Error> {
        let LaneState::Configuring {
            stream,
            epoch,
            until_us: None,
        } = self.state
        else {
            return Err(Error::WrongState);
        };
        self.counters.configurations += 1;
        self.state = LaneState::Configuring {
            stream,
            epoch,
            until_us: Some(now_us.saturating_add(CONFIGURE_TIMEOUT_US)),
        };
        Ok(())
    }
    /// The packet was admitted as a datagram (not a delivery receipt).
    pub fn packet_sent(&mut self, sequence: u64) -> Result<(), Error> {
        match self.state {
            LaneState::Active {
                stream,
                epoch,
                next,
            } if next == sequence => {
                self.counters.sent += 1;
                self.state = LaneState::Active {
                    stream,
                    epoch,
                    next: next + 1,
                };
                Ok(())
            }
            _ => Err(Error::WrongState),
        }
    }
    /// The stop record was accepted by the transport.
    pub fn stop_sent(&mut self) -> Result<(), Error> {
        let LaneState::Stopping { then, .. } = self.state else {
            return Err(Error::WrongState);
        };
        self.counters.stops += 1;
        self.state = match then {
            Then::Reconfigure => LaneState::Waiting,
            Then::Stop => LaneState::Stopped,
        };
        Ok(())
    }
    /// The viewer's `AudioConfigured`. Only the exact outstanding epoch with
    /// the exact advertised shape activates the lane, and only packets
    /// captured AFTER this point are sent. A stale epoch is refused and
    /// changes nothing; an explicit viewer refusal ends the lane.
    pub fn configured(&mut self, ack: AudioConfigured, ring: &AudioRing) -> Result<(), Error> {
        let LaneState::Configuring {
            stream,
            epoch,
            until_us: Some(_),
        } = self.state
        else {
            self.counters.stale_refused += 1;
            return Err(if self.last_epoch == Some(ack.generation) {
                Error::WrongState
            } else {
                Error::StaleGeneration
            });
        };
        if ack.generation != epoch || ack.direction != AudioDirection::Downlink {
            self.counters.stale_refused += 1;
            return Err(Error::StaleGeneration);
        }
        if !ack.accepted {
            self.state = LaneState::Stopped;
            return Ok(());
        }
        if ack.actual_channels != stream.channels
            || ack.actual_sample_rate != OPUS_SAMPLE_RATE
            || ack.actual_frame_duration_ms != stream.frame_duration_ms
        {
            self.state = LaneState::Stopping {
                epoch,
                reason: AudioStopReason::SessionEnded,
                then: Then::Stop,
            };
            return Err(Error::Invalid);
        }
        let next = ring.newest().map_or(0, |n| n + 1);
        self.counters.skipped_before_ack += u64::try_from(ring.len()).unwrap_or(u64::MAX);
        self.state = LaneState::Active {
            stream,
            epoch,
            next,
        };
        Ok(())
    }
    /// The viewer's own `AudioStop`: fences this lane immediately. A stop for
    /// another epoch is refused and changes nothing. `DeviceChanged` means the
    /// viewer reset its local output: a bounded number of fresh epochs follow;
    /// any other reason ends the lane.
    pub fn viewer_stopped(&mut self, stop: AudioStop) -> Result<(), Error> {
        let current = match self.state {
            LaneState::Configuring { epoch, .. }
            | LaneState::Active { epoch, .. }
            | LaneState::Stopping { epoch, .. } => epoch,
            LaneState::Waiting | LaneState::Stopped => {
                self.counters.stale_refused += 1;
                return Err(Error::StaleGeneration);
            }
        };
        if stop.generation != current || stop.direction != AudioDirection::Downlink {
            self.counters.stale_refused += 1;
            return Err(Error::StaleGeneration);
        }
        self.state = if stop.reason == AudioStopReason::DeviceChanged
            && self.counters.restarts < MAX_LANE_RESTARTS
        {
            self.counters.restarts += 1;
            LaneState::Waiting
        } else {
            LaneState::Stopped
        };
        Ok(())
    }
    /// Local teardown/departure: fence without sending anything further.
    pub fn close(&mut self) {
        self.state = LaneState::Stopped;
    }
}

#[cfg(test)]
mod tests;
