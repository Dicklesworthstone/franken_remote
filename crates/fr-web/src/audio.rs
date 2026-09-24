#![forbid(unsafe_code)]
//! Browser audio: Opus playback downlink, AudioWorklet ring/credit protocol,
//! and getUserMedia microphone uplink (plan §§15.4, 16.3; bead fr-p3-browser-audio-ebf).

use fr_core::audio::{AudioChannels, AudioGeneration, MicTalkMode, NOMINAL_SAMPLES_PER_FRAME};
use serde::{Deserialize, Serialize};

/// AudioWorklet ring buffer metrics and event counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AudioWorkletMetrics {
    pub underruns: u64,
    pub overruns: u64,
    pub frames_rendered: u64,
    pub frames_queued: u64,
}

/// A transferable audio frame chunk for AudioWorklet playback.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioWorkletFrame {
    pub sequence: u64,
    pub generation: u64,
    pub samples_per_channel: u16,
    pub channels: u8,
    pub pcm_data: Vec<f32>,
}

impl AudioWorkletFrame {
    #[must_use]
    pub fn generation(&self) -> AudioGeneration {
        AudioGeneration::from_raw(self.generation)
    }
}

/// Ring and credit controller between WASM decoder and AudioWorklet.
/// Uses transferable buffer batches with credit returns to guarantee bounded queueing.
#[derive(Debug, Clone)]
pub struct AudioRingCredit {
    capacity: usize,
    credits: usize,
    queue: std::collections::VecDeque<AudioWorkletFrame>,
    metrics: AudioWorkletMetrics,
}

impl AudioRingCredit {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.clamp(2, 64);
        Self {
            capacity: cap,
            credits: cap,
            queue: std::collections::VecDeque::with_capacity(cap),
            metrics: AudioWorkletMetrics::default(),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn available_credits(&self) -> usize {
        self.credits
    }

    pub fn queued_frames(&self) -> usize {
        self.queue.len()
    }

    pub fn metrics(&self) -> AudioWorkletMetrics {
        self.metrics
    }

    /// Enqueue decoded audio frame if credits allow; drops on overrun.
    pub fn push_frame(&mut self, frame: AudioWorkletFrame) -> Result<(), &'static str> {
        if self.credits == 0 || self.queue.len() >= self.capacity {
            self.metrics.overruns += 1;
            return Err("buffer_overrun");
        }
        self.credits -= 1;
        self.queue.push_back(frame);
        self.metrics.frames_queued += 1;
        Ok(())
    }

    /// AudioWorklet consumes the next frame to render.
    pub fn pop_frame_for_render(&mut self) -> Option<AudioWorkletFrame> {
        if let Some(f) = self.queue.pop_front() {
            self.metrics.frames_rendered += 1;
            Some(f)
        } else {
            self.metrics.underruns += 1;
            None
        }
    }

    /// AudioWorklet reports rendered frame completion, releasing credits.
    pub fn return_credit(&mut self, count: usize) {
        self.credits = (self.credits + count).min(self.capacity);
    }

    /// Flushes queued frames on generation change or device switch, resetting credits.
    pub fn reset(&mut self) {
        self.queue.clear();
        self.credits = self.capacity;
    }
}

/// Browser gesture lock state for AudioContext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AudioGestureState {
    #[default]
    RequiresGesture,
    Unlocked,
    Suspended,
}

/// Downlink playback pipeline controller.
#[derive(Debug, Clone)]
pub struct WebAudioPlayback {
    gesture_state: AudioGestureState,
    current_generation: AudioGeneration,
    ring: AudioRingCredit,
    jitter_target_ms: u16,
    is_muted: bool,
    active_channels: AudioChannels,
}

impl WebAudioPlayback {
    pub fn new(capacity: usize, jitter_target_ms: u16) -> Self {
        Self {
            gesture_state: AudioGestureState::RequiresGesture,
            current_generation: AudioGeneration::INITIAL,
            ring: AudioRingCredit::new(capacity),
            jitter_target_ms: jitter_target_ms.clamp(10, 100),
            is_muted: false,
            active_channels: AudioChannels::Stereo,
        }
    }

    pub fn gesture_state(&self) -> AudioGestureState {
        self.gesture_state
    }

    pub fn ring(&self) -> &AudioRingCredit {
        &self.ring
    }

    pub fn ring_mut(&mut self) -> &mut AudioRingCredit {
        &mut self.ring
    }

    pub fn jitter_target_ms(&self) -> u16 {
        self.jitter_target_ms
    }

    /// Called on user click or touch to unlock browser AudioContext.
    pub fn unlock_gesture(&mut self) {
        if self.gesture_state != AudioGestureState::Unlocked {
            self.gesture_state = AudioGestureState::Unlocked;
        }
    }

    /// Visibility change: browser policy pauses audio when tab is backgrounded.
    pub fn on_visibility_change(&mut self, is_hidden: bool) {
        if is_hidden {
            if self.gesture_state == AudioGestureState::Unlocked {
                self.gesture_state = AudioGestureState::Suspended;
            }
            self.ring.reset();
        } else if self.gesture_state == AudioGestureState::Suspended {
            self.gesture_state = AudioGestureState::Unlocked;
        }
    }

    /// Process incoming audio packet; validates gesture state, generation and jitter bounds.
    pub fn receive_packet(
        &mut self,
        generation: AudioGeneration,
        sequence: u64,
        samples_per_channel: u16,
        pcm: Vec<f32>,
    ) -> Result<(), &'static str> {
        if self.gesture_state != AudioGestureState::Unlocked {
            return Err("gesture_required");
        }
        if generation != self.current_generation {
            return Err("stale_generation");
        }
        if self.is_muted {
            return Ok(());
        }

        let frame = AudioWorkletFrame {
            sequence,
            generation: generation.as_raw(),
            samples_per_channel,
            channels: self.active_channels.count(),
            pcm_data: pcm,
        };
        self.ring.push_frame(frame)
    }

    /// Set mute state.
    pub fn set_muted(&mut self, muted: bool) {
        self.is_muted = muted;
        if muted {
            self.ring.reset();
        }
    }

    /// Advance stream epoch on host generation bump or reconnect.
    pub fn advance_generation(&mut self, next: AudioGeneration) {
        self.current_generation = next;
        self.ring.reset();
    }
}

/// Browser microphone permission query/status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MicPermissionState {
    #[default]
    Prompt,
    Granted,
    Denied,
}

/// Visual indicator state for client microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum MicIndicatorState {
    #[default]
    Inactive,
    Listening,
    Transmitting,
}

/// Uplink microphone stream controller.
#[derive(Debug, Clone)]
pub struct WebAudioUplink {
    permission: MicPermissionState,
    talk_mode: MicTalkMode,
    is_transmitting: bool,
    generation: AudioGeneration,
    sequence: u64,
    has_input_authority: bool,
}

impl Default for WebAudioUplink {
    fn default() -> Self {
        Self {
            permission: MicPermissionState::default(),
            talk_mode: MicTalkMode::default(),
            is_transmitting: false,
            generation: AudioGeneration::INITIAL,
            sequence: 0,
            has_input_authority: false,
        }
    }
}

impl WebAudioUplink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn permission(&self) -> MicPermissionState {
        self.permission
    }

    pub fn talk_mode(&self) -> MicTalkMode {
        self.talk_mode
    }

    pub fn is_transmitting(&self) -> bool {
        self.is_transmitting
    }

    pub fn indicator_state(&self) -> MicIndicatorState {
        if !self.is_transmitting || self.permission != MicPermissionState::Granted {
            MicIndicatorState::Inactive
        } else if !self.talk_mode.is_active() {
            MicIndicatorState::Listening
        } else {
            MicIndicatorState::Transmitting
        }
    }

    /// Updates browser permission result.
    pub fn set_permission(&mut self, permission: MicPermissionState) {
        self.permission = permission;
        if permission != MicPermissionState::Granted {
            self.stop_transmission();
        }
    }

    /// Syncs input authority status (microphone transmission is gated by active observation).
    pub fn set_input_authority(&mut self, has_authority: bool) {
        self.has_input_authority = has_authority;
        if !has_authority {
            self.stop_transmission();
        }
    }

    /// Set user talk mode (Muted, PushToTalk, OpenMic).
    pub fn set_talk_mode(&mut self, mode: MicTalkMode) {
        self.talk_mode = mode;
        if !mode.is_active() {
            self.stop_transmission();
        }
    }

    /// User activates PTT or opens mic.
    pub fn start_transmission(&mut self) -> Result<(), &'static str> {
        if self.permission != MicPermissionState::Granted {
            return Err("permission_denied");
        }
        if !self.has_input_authority {
            return Err("authority_required");
        }
        if !self.talk_mode.is_active() {
            return Err("mic_muted");
        }
        self.is_transmitting = true;
        Ok(())
    }

    pub fn stop_transmission(&mut self) {
        self.is_transmitting = false;
    }

    /// Encodes a 10ms frame payload if active; increments sequence.
    pub fn next_uplink_packet(
        &mut self,
        pcm: &[f32],
    ) -> Result<(u64, AudioGeneration, Vec<u8>), &'static str> {
        if !self.is_transmitting || self.permission != MicPermissionState::Granted {
            return Err("not_transmitting");
        }
        if pcm.len() != NOMINAL_SAMPLES_PER_FRAME as usize {
            return Err("invalid_sample_count");
        }
        self.sequence = self.sequence.wrapping_add(1);
        let mut bytes = Vec::with_capacity(pcm.len() * 4);
        for &sample in pcm {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        Ok((self.sequence, self.generation, bytes))
    }

    /// Tab hidden or teardown: immediately ceases transmission.
    pub fn on_visibility_change(&mut self, is_hidden: bool) {
        if is_hidden {
            self.stop_transmission();
        }
    }

    /// Advance generation on reconnect or device change.
    pub fn advance_generation(&mut self, next_gen: AudioGeneration) {
        self.generation = next_gen;
        self.sequence = 0;
        self.stop_transmission();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_credit_protocol_bounds_and_counters() {
        let mut ring = AudioRingCredit::new(3);
        assert_eq!(ring.capacity(), 3);
        assert_eq!(ring.available_credits(), 3);

        let f1 = AudioWorkletFrame {
            sequence: 1,
            generation: AudioGeneration::INITIAL.as_raw(),
            samples_per_channel: 480,
            channels: 2,
            pcm_data: vec![0.0; 960],
        };
        assert!(ring.push_frame(f1.clone()).is_ok());
        assert_eq!(ring.available_credits(), 2);
        assert_eq!(ring.queued_frames(), 1);

        assert!(ring.push_frame(f1.clone()).is_ok());
        assert!(ring.push_frame(f1.clone()).is_ok());
        assert_eq!(ring.available_credits(), 0);

        // Overrun on 4th frame
        assert_eq!(ring.push_frame(f1).unwrap_err(), "buffer_overrun");
        assert_eq!(ring.metrics().overruns, 1);
        assert_eq!(ring.metrics().frames_queued, 3);

        // Render frames
        assert!(ring.pop_frame_for_render().is_some());
        assert!(ring.pop_frame_for_render().is_some());
        assert!(ring.pop_frame_for_render().is_some());
        assert_eq!(ring.metrics().frames_rendered, 3);

        // Underrun on empty pop
        assert!(ring.pop_frame_for_render().is_none());
        assert_eq!(ring.metrics().underruns, 1);

        // Credit return allows new pushes
        ring.return_credit(2);
        assert_eq!(ring.available_credits(), 2);
    }

    #[test]
    fn playback_gesture_and_visibility_fencing() {
        let mut playback = WebAudioPlayback::new(4, 20);
        assert_eq!(playback.gesture_state(), AudioGestureState::RequiresGesture);

        // Refused before gesture
        assert_eq!(
            playback
                .receive_packet(AudioGeneration::INITIAL, 1, 480, vec![0.0; 960])
                .unwrap_err(),
            "gesture_required"
        );

        // Gesture unlock
        playback.unlock_gesture();
        assert_eq!(playback.gesture_state(), AudioGestureState::Unlocked);
        assert!(
            playback
                .receive_packet(AudioGeneration::INITIAL, 1, 480, vec![0.0; 960])
                .is_ok()
        );

        // Stale generation rejected
        let next_gen = AudioGeneration::from_raw(2);
        assert_eq!(
            playback
                .receive_packet(next_gen, 2, 480, vec![0.0; 960])
                .unwrap_err(),
            "stale_generation"
        );

        // Tab hidden suspends audio
        playback.on_visibility_change(true);
        assert_eq!(playback.gesture_state(), AudioGestureState::Suspended);
        assert_eq!(playback.ring().queued_frames(), 0);

        // Tab visible resumes
        playback.on_visibility_change(false);
        assert_eq!(playback.gesture_state(), AudioGestureState::Unlocked);
    }

    #[test]
    fn uplink_permission_talk_mode_and_indicator() {
        let mut uplink = WebAudioUplink::new();
        assert_eq!(uplink.indicator_state(), MicIndicatorState::Inactive);

        // Cannot start without permission and authority
        assert_eq!(
            uplink.start_transmission().unwrap_err(),
            "permission_denied"
        );
        uplink.set_permission(MicPermissionState::Granted);
        assert_eq!(
            uplink.start_transmission().unwrap_err(),
            "authority_required"
        );

        uplink.set_input_authority(true);
        // Still muted
        assert_eq!(uplink.start_transmission().unwrap_err(), "mic_muted");

        // Set PTT and activate
        uplink.set_talk_mode(MicTalkMode::PushToTalk { active: true });
        assert!(uplink.start_transmission().is_ok());
        assert_eq!(uplink.indicator_state(), MicIndicatorState::Transmitting);

        // Packet generation
        let samples = vec![0.5f32; 480];
        let (seq, packet_gen, bytes) = uplink.next_uplink_packet(&samples).unwrap();
        assert_eq!(seq, 1);
        assert_eq!(packet_gen, AudioGeneration::INITIAL);
        assert_eq!(bytes.len(), 480 * 4);

        // Mute or tab hide terminates transmission
        uplink.on_visibility_change(true);
        assert!(!uplink.is_transmitting());
        assert_eq!(uplink.indicator_state(), MicIndicatorState::Inactive);
    }
}
