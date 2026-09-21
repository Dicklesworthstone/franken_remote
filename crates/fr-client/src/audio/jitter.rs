#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_lossless,
    clippy::cast_sign_loss,
    clippy::large_enum_variant,
    clippy::large_stack_arrays,
    clippy::collapsible_if,
    clippy::needless_range_loop,
    clippy::manual_let_else,
    clippy::single_match_else,
    clippy::comparison_chain
)]
//! Bounded client audio jitter buffer with packet loss concealment (PLC) and strict ceiling.
//!
//! Conforms to plan section 15.4:
//! - Small bounded adaptive jitter buffer.
//! - Strict ceiling of [`MAX_JITTER_CEILING_MS`] (100 ms).
//! - Late packets arriving behind playout position are discarded immediately.
//! - Out-of-order packets are reordered by sequence number.
//! - Packet loss triggers PLC without blocking video or advancing fake clocks.
//! - Device switch or reconnect immediately resets the generation and clears old samples,
//!   preventing stale buffered audio from ever playing after resume.

use fr_core::audio::{
    AudioGeneration, MAX_JITTER_CEILING_MS, NOMINAL_SAMPLES_PER_FRAME, OPUS_SAMPLE_RATE,
};
use fr_media::audio::AudioAccessUnit;

/// Maximum number of queued packets in the jitter buffer.
/// At 10 ms per packet, 16 packets represents 160 ms capacity, clamped by the 100 ms ceiling.
pub const JITTER_BUFFER_CAPACITY: usize = 16;

/// Playout outcome when draining the jitter buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum JitterDrainResult {
    /// Playout packet is ready.
    Packet(AudioAccessUnit),
    /// A packet was lost; the decoder should synthesize concealment waveform (PLC).
    Plc {
        missing_sequence: u64,
        duration_samples: u16,
    },
    /// Jitter buffer underrun or waiting for initial prebuffer depth.
    Underrun,
}

/// Jitter buffer performance and loss diagnostic counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct JitterBufferMetrics {
    pub packets_received: u64,
    pub packets_played: u64,
    pub packets_late_discarded: u64,
    pub packets_duplicate_discarded: u64,
    pub plc_concealment_events: u64,
    pub underrun_count: u64,
    pub current_depth_ms: u16,
}

/// Small bounded adaptive jitter buffer.
pub struct AudioJitterBuffer {
    generation: AudioGeneration,
    target_depth_ms: u16,
    ceiling_ms: u16,
    slots: [Option<AudioAccessUnit>; JITTER_BUFFER_CAPACITY],
    slot_count: usize,
    next_expected_sequence: Option<u64>,
    playout_sample_timestamp: u64,
    is_prebuffering: bool,
    metrics: JitterBufferMetrics,
}

impl AudioJitterBuffer {
    /// Creates a new jitter buffer bound to an initial audio generation and target delay.
    #[must_use]
    pub fn new(generation: AudioGeneration, target_depth_ms: u16) -> Self {
        let clamped_target = target_depth_ms.clamp(10, MAX_JITTER_CEILING_MS);
        Self {
            generation,
            target_depth_ms: clamped_target,
            ceiling_ms: MAX_JITTER_CEILING_MS,
            slots: [None; JITTER_BUFFER_CAPACITY],
            slot_count: 0,
            next_expected_sequence: None,
            playout_sample_timestamp: 0,
            is_prebuffering: true,
            metrics: JitterBufferMetrics::default(),
        }
    }

    /// Resets the jitter buffer on device change, reconnect, or generation switch.
    ///
    /// Drops all buffered packets so obsolete audio cannot play after resume (Plan §15.4).
    pub fn reset_generation(&mut self, new_generation: AudioGeneration) {
        self.generation = new_generation;
        self.slots.fill(None);
        self.slot_count = 0;
        self.next_expected_sequence = None;
        self.playout_sample_timestamp = 0;
        self.is_prebuffering = true;
        self.metrics.current_depth_ms = 0;
    }

    #[must_use]
    pub const fn generation(&self) -> AudioGeneration {
        self.generation
    }

    #[must_use]
    pub const fn target_depth_ms(&self) -> u16 {
        self.target_depth_ms
    }

    #[must_use]
    pub const fn metrics(&self) -> JitterBufferMetrics {
        self.metrics
    }

    #[must_use]
    pub const fn queued_packet_count(&self) -> usize {
        self.slot_count
    }

    /// Computes current buffered depth in milliseconds.
    #[must_use]
    pub fn current_depth_ms(&self) -> u16 {
        let mut total_samples: u32 = 0;
        for slot in self.slots.iter().flatten() {
            total_samples += slot.duration_samples() as u32;
        }
        let ms = (total_samples * 1000) / OPUS_SAMPLE_RATE;
        ms.min(u32::from(u16::MAX)) as u16
    }

    /// Submits a packet to the jitter buffer.
    ///
    /// Validates generation, discards duplicates, drops late packets, and orders out-of-order arrivals.
    pub fn push_packet(&mut self, packet: AudioAccessUnit) -> bool {
        // Enforce audio generation fencing
        if packet.generation() != self.generation {
            return false;
        }

        self.metrics.packets_received += 1;

        // Late packet check: if we already played past this sequence, discard immediately
        if let Some(expected_seq) = self.next_expected_sequence {
            if packet.sequence() < expected_seq {
                self.metrics.packets_late_discarded += 1;
                return false;
            }
        }

        // Duplicate check
        for slot in self.slots.iter().flatten() {
            if slot.sequence() == packet.sequence() {
                self.metrics.packets_duplicate_discarded += 1;
                return false;
            }
        }

        // Check if buffer is full or would exceed strict ceiling
        let incoming_duration_ms = (packet.duration_samples() as u32 * 1000) / OPUS_SAMPLE_RATE;
        if self.slot_count >= JITTER_BUFFER_CAPACITY
            || self.current_depth_ms() + (incoming_duration_ms as u16) > self.ceiling_ms
        {
            // Drop oldest packet if needed to protect latency ceiling
            self.drop_oldest_packet();
        }

        // Insert in ascending sequence order
        let mut insert_pos = self.slot_count;
        for i in 0..self.slot_count {
            if let Some(slot) = &self.slots[i] {
                if packet.sequence() < slot.sequence() {
                    insert_pos = i;
                    break;
                }
            }
        }

        for i in (insert_pos..self.slot_count).rev() {
            self.slots[i + 1] = self.slots[i].take();
        }
        self.slots[insert_pos] = Some(packet);
        self.slot_count += 1;

        self.metrics.current_depth_ms = self.current_depth_ms();

        // Release prebuffering once target depth is reached
        if self.is_prebuffering && self.metrics.current_depth_ms >= self.target_depth_ms {
            self.is_prebuffering = false;
        }

        true
    }

    /// Drains the next playable unit from the jitter buffer.
    ///
    /// Returns:
    /// - [`JitterDrainResult::Packet`] if the expected packet is present.
    /// - [`JitterDrainResult::Plc`] if a sequence hole was encountered.
    /// - [`JitterDrainResult::Underrun`] if insufficient packets are available.
    pub fn drain(&mut self) -> JitterDrainResult {
        if self.is_prebuffering || self.slot_count == 0 {
            self.metrics.underrun_count += 1;
            return JitterDrainResult::Underrun;
        }

        let first = match self.slots[0] {
            Some(pkt) => pkt,
            None => {
                self.metrics.underrun_count += 1;
                return JitterDrainResult::Underrun;
            }
        };

        match self.next_expected_sequence {
            None => {
                // First packet drained: anchor expected sequence
                let packet = self.pop_first_packet();
                self.next_expected_sequence = Some(packet.sequence() + 1);
                self.playout_sample_timestamp = packet.timestamp_samples() + (packet.duration_samples() as u64);
                self.metrics.packets_played += 1;
                self.metrics.current_depth_ms = self.current_depth_ms();
                JitterDrainResult::Packet(packet)
            }
            Some(expected_seq) => {
                if first.sequence() == expected_seq {
                    let packet = self.pop_first_packet();
                    self.next_expected_sequence = Some(expected_seq + 1);
                    self.playout_sample_timestamp = packet.timestamp_samples() + (packet.duration_samples() as u64);
                    self.metrics.packets_played += 1;
                    self.metrics.current_depth_ms = self.current_depth_ms();
                    JitterDrainResult::Packet(packet)
                } else if first.sequence() > expected_seq {
                    // Sequence hole: missing packet! Trigger PLC for nominal frame
                    self.next_expected_sequence = Some(expected_seq + 1);
                    self.playout_sample_timestamp += NOMINAL_SAMPLES_PER_FRAME as u64;
                    self.metrics.plc_concealment_events += 1;
                    JitterDrainResult::Plc {
                        missing_sequence: expected_seq,
                        duration_samples: NOMINAL_SAMPLES_PER_FRAME,
                    }
                } else {
                    // Stale / late packet reached the front unexpectedly: drop and retry
                    let _ = self.pop_first_packet();
                    self.metrics.packets_late_discarded += 1;
                    self.drain()
                }
            }
        }
    }

    fn pop_first_packet(&mut self) -> AudioAccessUnit {
        let pkt = self.slots[0].take().expect("slot 0 must be populated");
        for i in 1..self.slot_count {
            self.slots[i - 1] = self.slots[i].take();
        }
        self.slot_count -= 1;
        pkt
    }

    fn drop_oldest_packet(&mut self) {
        if self.slot_count > 0 {
            let _ = self.pop_first_packet();
            self.metrics.packets_late_discarded += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::audio::AudioDirection;

    fn make_test_packet(generation: AudioGeneration, sequence: u64, timestamp: u64) -> AudioAccessUnit {
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
            JitterDrainResult::Plc { missing_sequence, duration_samples } => {
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
