#![forbid(unsafe_code)]
//! Bounded, negotiated audio reordering and sample-timed concealment.
//!
//! The queue is not an audio clock or an authority grant. A playout owner must
//! pace drains, enforce original deadlines and recheck audio permission. Before
//! the first decode, obsolete startup packets may be evicted. Afterwards a
//! discontinuity fences the generation instead of skipping decoder history or
//! generating an unbounded train of concealment frames.

use fr_core::audio::{
    AudioDirection, AudioGeneration, AudioStreamConfig, MAX_JITTER_CEILING_MS,
    NOMINAL_SAMPLES_PER_FRAME, OPUS_SAMPLE_RATE,
};
use fr_media::audio::AudioAccessUnit;

pub const JITTER_BUFFER_CAPACITY: usize = 16;
const CEILING_SAMPLES: u64 = OPUS_SAMPLE_RATE as u64 * MAX_JITTER_CEILING_MS as u64 / 1000;

#[derive(Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum JitterDrainResult {
    Packet(AudioAccessUnit),
    /// Concealment is an explicit missing sequence, never observed source audio.
    Plc {
        missing_sequence: u64,
        duration_samples: u16,
    },
    Underrun,
}

/// Sanitized metadata errors. A terminal error remains visible until a strictly
/// newer generation is configured; rejected packets never repair that state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitterError {
    InvalidConfiguration,
    WrongDirection,
    WrongDuration,
    Timeline,
    CounterOverflow,
    WindowExceeded,
    ConcealmentExhausted,
    GenerationNotAdvanced,
    Stopped,
}
impl core::fmt::Display for JitterError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "audio-jitter: {self:?}")
    }
}
impl std::error::Error for JitterError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JitterBufferMetrics {
    pub packets_received: u64,
    pub packets_played: u64,
    pub packets_late_discarded: u64,
    pub packets_duplicate_discarded: u64,
    pub plc_concealment_events: u64,
    pub underrun_count: u64,
    pub current_depth_ms: u16,
    pub packets_refused: u64,
    pub discontinuities: u64,
}

pub struct AudioJitterBuffer {
    generation: AudioGeneration,
    direction: AudioDirection,
    duration_samples: u16,
    target_depth_ms: u16,
    slots: [Option<AudioAccessUnit>; JITTER_BUFFER_CAPACITY],
    slot_count: usize,
    next: Option<(u64, u64)>, // Next sequence and sample position, not wall time.
    concealed_samples: u64,
    error: Option<JitterError>,
    is_prebuffering: bool,
    metrics: JitterBufferMetrics,
}

impl AudioJitterBuffer {
    /// Compatibility constructor for nominal 10 ms downlink packets. Other
    /// admitted formats must use `with_config`, not guessed PLC durations.
    // Fixed 16-packet storage; constructed outside the audio callback, no hot-path allocation.
    #[allow(clippy::large_stack_arrays)]
    pub fn new(generation: AudioGeneration, target_depth_ms: u16) -> Self {
        Self {
            generation,
            direction: AudioDirection::Downlink,
            duration_samples: NOMINAL_SAMPLES_PER_FRAME,
            target_depth_ms: target_depth_ms.clamp(10, MAX_JITTER_CEILING_MS),
            slots: [None; JITTER_BUFFER_CAPACITY],
            slot_count: 0,
            next: None,
            concealed_samples: 0,
            error: None,
            is_prebuffering: true,
            metrics: JitterBufferMetrics::default(),
        }
    }

    /// Retain the negotiated direction and packet duration. A 120 ms packet
    /// cannot fit the 100 ms playout ceiling; reject rather than weaken either
    /// the queue bound or negotiation. Small packets also obey the 16-slot cap.
    pub fn with_config(config: AudioStreamConfig) -> Result<Self, JitterError> {
        if !matches!(config.frame_duration_ms(), 5 | 10 | 20 | 40 | 60 | 80 | 100) {
            return Err(JitterError::InvalidConfiguration);
        }
        let packets = config
            .jitter_target_ms()
            .div_ceil(config.frame_duration_ms());
        if usize::from(packets) > JITTER_BUFFER_CAPACITY
            || u32::from(packets) * u32::from(config.frame_duration_ms())
                > u32::from(MAX_JITTER_CEILING_MS)
        {
            return Err(JitterError::InvalidConfiguration);
        }
        let mut buffer = Self::new(config.generation(), config.jitter_target_ms());
        buffer.direction = config.direction();
        buffer.duration_samples = u16::try_from(config.expected_samples_per_frame())
            .map_err(|_| JitterError::InvalidConfiguration)?;
        buffer.target_depth_ms = config.jitter_target_ms();
        Ok(buffer)
    }

    /// Invalid resets clear sound but do not lower the retired-generation floor.
    pub fn reset_generation(&mut self, new_generation: AudioGeneration) {
        if !new_generation.supersedes(self.generation) {
            self.fail(JitterError::GenerationNotAdvanced);
            return;
        }
        self.clear();
        self.generation = new_generation;
        self.error = None;
    }
    pub fn stop(&mut self) {
        self.fail(JitterError::Stopped);
    }
    pub const fn generation(&self) -> AudioGeneration {
        self.generation
    }
    pub const fn target_depth_ms(&self) -> u16 {
        self.target_depth_ms
    }
    pub const fn metrics(&self) -> JitterBufferMetrics {
        self.metrics
    }
    pub(super) fn contains_sequence(&self, sequence: u64) -> bool {
        self.slots[..self.slot_count]
            .iter()
            .flatten()
            .any(|packet| packet.sequence() == sequence)
    }
    pub const fn queued_packet_count(&self) -> usize {
        self.slot_count
    }
    pub const fn error(&self) -> Option<JitterError> {
        self.error
    }
    /// The next audio sample position, only after a real packet was drained.
    pub const fn next_sample_timestamp(&self) -> Option<u64> {
        match self.next {
            Some((_, at)) => Some(at),
            None => None,
        }
    }
    pub fn current_depth_ms(&self) -> u16 {
        // At most 16 admitted packets of at most 100 ms; no untrusted arithmetic.
        let samples = u32::try_from(self.slot_count).expect("bounded jitter capacity")
            * u32::from(self.duration_samples);
        u16::try_from(samples * 1000 / OPUS_SAMPLE_RATE).unwrap_or(u16::MAX)
    }

    /// Legacy boolean admission; `try_push_packet` preserves refusal causes.
    pub fn push_packet(&mut self, packet: AudioAccessUnit) -> bool {
        self.try_push_packet(packet).unwrap_or(false)
    }
    pub fn try_push_packet(&mut self, packet: AudioAccessUnit) -> Result<bool, JitterError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if packet.generation() != self.generation {
            return Ok(false);
        }
        self.metrics.packets_received = self.metrics.packets_received.saturating_add(1);
        let result = self.insert(&packet);
        if result.is_err() {
            self.metrics.packets_refused = self.metrics.packets_refused.saturating_add(1);
        }
        result
    }

    fn insert(&mut self, packet: &AudioAccessUnit) -> Result<bool, JitterError> {
        if packet.direction() != self.direction {
            return Err(JitterError::WrongDirection);
        }
        if packet.duration_samples() != self.duration_samples {
            return Err(JitterError::WrongDuration);
        }
        let end = packet
            .timestamp_samples()
            .checked_add(u64::from(self.duration_samples))
            .ok_or(JitterError::CounterOverflow)?;
        packet
            .sequence()
            .checked_add(1)
            .ok_or(JitterError::CounterOverflow)?;
        if self.next.is_some_and(|(seq, _)| packet.sequence() < seq) {
            self.discard_late();
            return Ok(false);
        }
        if self.slots[..self.slot_count]
            .iter()
            .flatten()
            .any(|p| p.sequence() == packet.sequence())
        {
            self.metrics.packets_duplicate_discarded =
                self.metrics.packets_duplicate_discarded.saturating_add(1);
            return Ok(false);
        }
        // Every accepted packet lies on one fixed negotiated sequence/sample
        // timeline. Capture gaps or duration changes require a new configuration;
        // they are not permission to fabricate an inferred packet history.
        if let Some(anchor) = self
            .next
            .or_else(|| self.slots[0].map(|p| (p.sequence(), p.timestamp_samples())))
        {
            let distance = packet
                .sequence()
                .abs_diff(anchor.0)
                .checked_mul(u64::from(self.duration_samples))
                .ok_or(JitterError::CounterOverflow)?;
            let expected = if packet.sequence() >= anchor.0 {
                anchor.1.checked_add(distance)
            } else {
                anchor.1.checked_sub(distance)
            };
            if expected != Some(packet.timestamp_samples()) {
                return Err(JitterError::Timeline);
            }
        }
        // Sample span includes holes, not just payload duration. After decode
        // starts, evicting an expected packet cannot silently skip codec state.
        while self.slot_count > 0 || self.next.is_some() {
            let first = self
                .next
                .map(|(_, at)| at)
                .or_else(|| self.slots[0].map(|p| p.timestamp_samples()))
                .unwrap_or(packet.timestamp_samples())
                .min(packet.timestamp_samples());
            let last = self.slots[..self.slot_count]
                .last()
                .and_then(Option::as_ref)
                .map_or(end, |p| {
                    p.timestamp_samples() + u64::from(self.duration_samples)
                })
                .max(end);
            if self.slot_count < JITTER_BUFFER_CAPACITY && last - first <= CEILING_SAMPLES {
                break;
            }
            if self.next.is_some() {
                self.fail(JitterError::WindowExceeded);
                return Err(JitterError::WindowExceeded);
            }
            if self.slots[0].is_some_and(|p| packet.sequence() < p.sequence()) {
                self.discard_late();
                return Ok(false);
            }
            let _ = self.pop_first_packet();
            self.discard_late();
        }
        let pos = self.slots[..self.slot_count]
            .iter()
            .position(|p| p.is_some_and(|p| p.sequence() > packet.sequence()))
            .unwrap_or(self.slot_count);
        for i in (pos..self.slot_count).rev() {
            self.slots[i + 1] = self.slots[i].take();
        }
        self.slots[pos] = Some(*packet);
        self.slot_count += 1;
        self.update_depth();
        if self.metrics.current_depth_ms >= self.target_depth_ms {
            self.is_prebuffering = false;
        }
        Ok(true)
    }

    /// Compatibility drain: a fenced stream is silent. Integrations must use
    /// `drain_checked` (or inspect `error`) to request explicit reconfiguration.
    pub fn drain(&mut self) -> JitterDrainResult {
        self.drain_checked().unwrap_or(JitterDrainResult::Underrun)
    }
    pub fn drain_checked(&mut self) -> Result<JitterDrainResult, JitterError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.is_prebuffering || self.slot_count == 0 {
            return Ok(self.underrun());
        }
        let first = self.slots[0].expect("occupied jitter prefix");
        if self.next.is_some_and(|(seq, _)| seq < first.sequence()) {
            return self.conceal();
        }
        let packet = self.pop_first_packet();
        // Checked at admission, including the last representable packet.
        self.next = Some((
            packet.sequence() + 1,
            packet.timestamp_samples() + u64::from(self.duration_samples),
        ));
        self.concealed_samples = 0;
        self.metrics.packets_played = self.metrics.packets_played.saturating_add(1);
        Ok(JitterDrainResult::Packet(packet))
    }
    /// Call once per due audio-clock frame, never in a free-running drain loop.
    /// Trailing loss receives the same finite concealment budget as an interior
    /// hole. No PLC is produced before the first real packet anchors the codec.
    pub fn drain_for_playout(&mut self) -> Result<JitterDrainResult, JitterError> {
        if self.error.is_none() && self.slot_count == 0 && self.next.is_some() {
            self.conceal()
        } else {
            self.drain_checked()
        }
    }
    fn conceal(&mut self) -> Result<JitterDrainResult, JitterError> {
        let Some((sequence, at)) = self.next else {
            return Ok(self.underrun());
        };
        let samples = u64::from(self.duration_samples);
        let concealed = self.concealed_samples + samples;
        if concealed > CEILING_SAMPLES {
            self.fail(JitterError::ConcealmentExhausted);
            return Err(JitterError::ConcealmentExhausted);
        }
        let Some(next) = sequence.checked_add(1).zip(at.checked_add(samples)) else {
            self.fail(JitterError::CounterOverflow);
            return Err(JitterError::CounterOverflow);
        };
        self.next = Some(next);
        self.concealed_samples = concealed;
        self.metrics.plc_concealment_events = self.metrics.plc_concealment_events.saturating_add(1);
        Ok(JitterDrainResult::Plc {
            missing_sequence: sequence,
            duration_samples: self.duration_samples,
        })
    }
    fn clear(&mut self) {
        self.slots.fill(None);
        self.slot_count = 0;
        self.next = None;
        self.concealed_samples = 0;
        self.is_prebuffering = true;
        self.metrics.current_depth_ms = 0;
    }
    fn fail(&mut self, error: JitterError) {
        self.clear();
        if self.error.is_none() {
            self.metrics.discontinuities = self.metrics.discontinuities.saturating_add(1);
            self.error = Some(error);
        }
    }
    fn underrun(&mut self) -> JitterDrainResult {
        self.metrics.underrun_count = self.metrics.underrun_count.saturating_add(1);
        JitterDrainResult::Underrun
    }
    fn discard_late(&mut self) {
        self.metrics.packets_late_discarded = self.metrics.packets_late_discarded.saturating_add(1);
    }
    fn update_depth(&mut self) {
        self.metrics.current_depth_ms = self.current_depth_ms();
    }
    fn pop_first_packet(&mut self) -> AudioAccessUnit {
        let packet = self.slots[0].take().expect("occupied jitter prefix");
        for i in 1..self.slot_count {
            self.slots[i - 1] = self.slots[i].take();
        }
        self.slot_count -= 1;
        self.update_depth();
        packet
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::audio::AudioDirection;

    fn make_test_packet(
        generation: AudioGeneration,
        sequence: u64,
        timestamp: u64,
    ) -> AudioAccessUnit {
        let payload = [0x42u8; 16];
        AudioAccessUnit::new(
            AudioDirection::Downlink,
            generation,
            sequence,
            timestamp,
            480,
            false,
            &payload,
        )
        .unwrap()
    }

    #[test]
    fn prebuffering_and_in_order_drain() {
        let generation = AudioGeneration::INITIAL;
        let mut jb = AudioJitterBuffer::new(generation, 20); // 20 ms target = 2 packets

        let p0 = make_test_packet(generation, 0, 0);
        let p1 = make_test_packet(generation, 1, 480);
        let p2 = make_test_packet(generation, 2, 960);

        assert!(jb.push_packet(p0));
        // Only 10 ms queued, target is 20 ms, so still prebuffering
        assert_eq!(jb.drain(), JitterDrainResult::Underrun);

        assert!(jb.push_packet(p1));
        // Now 20 ms queued, prebuffering clears!
        match jb.drain() {
            JitterDrainResult::Packet(p) => assert_eq!(p.sequence(), 0),
            other => panic!("expected Packet(0), got {other:?}"),
        }

        assert!(jb.push_packet(p2));
        match jb.drain() {
            JitterDrainResult::Packet(p) => assert_eq!(p.sequence(), 1),
            other => panic!("expected Packet(1), got {other:?}"),
        }
    }

    #[test]
    fn out_of_order_packets_are_reordered() {
        let generation = AudioGeneration::INITIAL;
        let mut jb = AudioJitterBuffer::new(generation, 20);

        // Push packet 1 before packet 0
        let p1 = make_test_packet(generation, 1, 480);
        let p0 = make_test_packet(generation, 0, 0);

        assert!(jb.push_packet(p1));
        assert!(jb.push_packet(p0));

        // Playout must deliver sequence 0 first
        match jb.drain() {
            JitterDrainResult::Packet(p) => assert_eq!(p.sequence(), 0),
            other => panic!("expected Packet(0), got {other:?}"),
        }
        match jb.drain() {
            JitterDrainResult::Packet(p) => assert_eq!(p.sequence(), 1),
            other => panic!("expected Packet(1), got {other:?}"),
        }
    }

    #[test]
    fn packet_loss_triggers_plc() {
        let generation = AudioGeneration::INITIAL;
        let mut jb = AudioJitterBuffer::new(generation, 20);

        let p0 = make_test_packet(generation, 0, 0);
        let p2 = make_test_packet(generation, 2, 960); // sequence 1 missing!

        assert!(jb.push_packet(p0));
        assert!(jb.push_packet(p2));

        // Drain 0
        match jb.drain() {
            JitterDrainResult::Packet(p) => assert_eq!(p.sequence(), 0),
            other => panic!("expected Packet(0), got {other:?}"),
        }

        // Drain next: 1 is missing, should trigger PLC
        match jb.drain() {
            JitterDrainResult::Plc {
                missing_sequence,
                duration_samples,
            } => {
                assert_eq!(missing_sequence, 1);
                assert_eq!(duration_samples, 480);
            }
            other => panic!("expected Plc, got {other:?}"),
        }

        // Drain next: 2 is present, should playout
        match jb.drain() {
            JitterDrainResult::Packet(p) => assert_eq!(p.sequence(), 2),
            other => panic!("expected Packet(2), got {other:?}"),
        }
    }

    #[test]
    fn late_packet_is_discarded() {
        let generation = AudioGeneration::INITIAL;
        let mut jb = AudioJitterBuffer::new(generation, 20);

        let p0 = make_test_packet(generation, 0, 0);
        let p1 = make_test_packet(generation, 1, 480);
        assert!(jb.push_packet(p0));
        assert!(jb.push_packet(p1));

        let _ = jb.drain(); // drained 0
        let _ = jb.drain(); // drained 1

        // Now packet 0 arrives again or late packet 0 arrives: must be discarded
        let late_p0 = make_test_packet(generation, 0, 0);
        assert!(!jb.push_packet(late_p0));
        assert_eq!(jb.metrics().packets_late_discarded, 1);
    }

    #[test]
    fn generation_reset_drops_stale_buffered_audio() {
        let gen1 = AudioGeneration::INITIAL;
        let mut jb = AudioJitterBuffer::new(gen1, 20);

        let p0 = make_test_packet(gen1, 0, 0);
        let p1 = make_test_packet(gen1, 1, 480);
        assert!(jb.push_packet(p0));
        assert!(jb.push_packet(p1));
        assert_eq!(jb.queued_packet_count(), 2);

        // Advance generation (device switch / reconnect)
        let gen2 = gen1.next().expect("valid next generation");
        jb.reset_generation(gen2);

        // Queued packets from old generation must be gone
        assert_eq!(jb.queued_packet_count(), 0);
        assert_eq!(jb.drain(), JitterDrainResult::Underrun);

        // Packets from old generation are rejected
        let stale_packet = make_test_packet(gen1, 2, 960);
        assert!(!jb.push_packet(stale_packet));

        // Packets from new generation are accepted
        let fresh_packet = make_test_packet(gen2, 0, 0);
        assert!(jb.push_packet(fresh_packet));
        assert_eq!(jb.queued_packet_count(), 1);
    }
}
