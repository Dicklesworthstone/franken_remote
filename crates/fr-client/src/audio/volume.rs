#![forbid(unsafe_code)]
#![allow(clippy::cast_possible_truncation, clippy::float_cmp)]
//! Client-side volume control and instant mute for remote playback audio (Plan §15.4).
//!
//! # Invariants:
//! - Local-only control: volume adjustments and instant mute never require a host round trip.
//! - Saturating arithmetic prevents audio clipping or wrap-around distortion.
//! - Settings can be persisted per remote host identifier.

use fr_media::audio::AudioPcmFrame;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

// Admitted nonnegative f32 gains leave the sign bit free. One atomic holds both
// fields so the output takes a coherent snapshot, and concurrent changes to gain
// and mute cannot overwrite each other. No PCM, commands or permission live here.
const MUTED: u32 = 1 << 31;
fn gain(volume: f32) -> f32 {
    if volume.is_nan() {
        0.0
    } else {
        volume.clamp(0.0, 1.0)
    }
    .abs()
}

/// Default initial volume level (100%).
pub const DEFAULT_VOLUME: f32 = 1.0;

/// Local client audio volume and mute controller. Clones intentionally control
/// the SAME playback preference, across the shell and its output worker. A new
/// connection creates a new control; this handle cannot enable or revive audio.
/// All setters are allocation-free and never wait for a lock or host response.
#[derive(Debug, Clone)]
pub struct AudioVolumeControl {
    state: Arc<AtomicU32>,
}
impl PartialEq for AudioVolumeControl {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
            || self.state.load(Ordering::Acquire) == other.state.load(Ordering::Acquire)
    }
}
impl Default for AudioVolumeControl {
    fn default() -> Self {
        Self::new()
    }
}
impl AudioVolumeControl {
    /// Creates an independent unmuted 100% controller, not a global preference.
    #[must_use]
    pub fn new() -> Self {
        Self::from_parts(DEFAULT_VOLUME, false)
    }
    /// Creates an independent controller. Non-finite NaN is conservative silence;
    /// infinities and finite values saturate to the documented [0, 1] range.
    #[must_use]
    pub fn from_parts(volume: f32, muted: bool) -> Self {
        Self {
            state: Arc::new(AtomicU32::new(
                gain(volume).to_bits() | if muted { MUTED } else { 0 },
            )),
        }
    }
    /// One coherent value for rendering or persistence, never another live handle.
    #[must_use]
    pub fn snapshot(&self) -> (f32, bool) {
        let state = self.state.load(Ordering::Acquire);
        (f32::from_bits(state & !MUTED), state & MUTED != 0)
    }
    #[must_use]
    pub fn volume(&self) -> f32 {
        self.snapshot().0
    }
    /// Preserve a simultaneous mute update. Clones share this change immediately;
    /// the output observes it at its next submission, not when decode begins.
    pub fn set_volume(&mut self, volume: f32) {
        let bits = gain(volume).to_bits();
        let _ = self
            .state
            .try_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some((old & MUTED) | bits)
            });
    }
    #[must_use]
    pub fn is_muted(&self) -> bool {
        self.snapshot().1
    }
    pub fn set_muted(&mut self, muted: bool) {
        if muted {
            self.state.fetch_or(MUTED, Ordering::AcqRel);
        } else {
            self.state.fetch_and(!MUTED, Ordering::AcqRel);
        }
    }
    /// Local-only toggle; does not discard codec history or change admission.
    pub fn toggle_mute(&mut self) -> bool {
        self.state.fetch_xor(MUTED, Ordering::AcqRel) & MUTED == 0
    }
    #[must_use]
    pub fn effective_gain(&self) -> f32 {
        let (volume, muted) = self.snapshot();
        if muted { 0.0 } else { volume }
    }

    /// Applies volume gain or mute in-place to an [`AudioPcmFrame`] using saturating arithmetic.
    pub fn apply_to_pcm(&self, frame: &mut AudioPcmFrame) {
        let gain = self.effective_gain();
        if gain == 1.0 {
            return;
        }

        let samples = frame.samples_mut();
        if gain == 0.0 {
            samples.fill(0);
            return;
        }

        for sample in samples.iter_mut() {
            let scaled = (f32::from(*sample) * gain).round();
            *sample = scaled.clamp(-32768.0, 32767.0) as i16;
        }
    }
}

/// Persistent audio settings associated with a specific remote host.
#[derive(Debug, Clone, PartialEq)]
pub struct HostAudioSettings {
    pub host_id: String,
    pub volume: f32,
    pub muted: bool,
}

impl HostAudioSettings {
    #[must_use]
    pub fn new(host_id: impl Into<String>, volume: f32, muted: bool) -> Self {
        Self {
            host_id: host_id.into(),
            volume: volume.clamp(0.0, 1.0),
            muted,
        }
    }

    /// Formats settings as a simple single-line string: `host_id=volume,muted`.
    #[must_use]
    pub fn serialize_line(&self) -> String {
        format!(
            "{}={:.4},{}",
            self.host_id,
            self.volume,
            i32::from(self.muted)
        )
    }

    /// Parses a single-line string into [`HostAudioSettings`].
    #[must_use]
    pub fn parse_line(line: &str) -> Option<Self> {
        let (host, rest) = line.split_once('=')?;
        let (vol_str, mute_str) = rest.split_once(',')?;
        let volume: f32 = vol_str.trim().parse().ok()?;
        let muted = match mute_str.trim() {
            "1" | "true" => true,
            "0" | "false" => false,
            _ => return None,
        };
        Some(Self::new(host.trim(), volume, muted))
    }
}

/// In-memory and file-compatible store for per-host audio settings.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostAudioStore {
    records: Vec<HostAudioSettings>,
}

impl HostAudioStore {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            records: Vec::new(),
        }
    }

    /// Retrieves volume and mute controller for a given host, or default if unset.
    #[must_use]
    pub fn get_or_default(&self, host_id: &str) -> AudioVolumeControl {
        if let Some(entry) = self.records.iter().find(|r| r.host_id == host_id) {
            AudioVolumeControl::from_parts(entry.volume, entry.muted)
        } else {
            AudioVolumeControl::new()
        }
    }

    /// Saves or updates the volume settings for a given host.
    pub fn save_settings(&mut self, host_id: &str, control: &AudioVolumeControl) {
        let (volume, muted) = control.snapshot();
        if let Some(entry) = self.records.iter_mut().find(|r| r.host_id == host_id) {
            entry.volume = volume;
            entry.muted = muted;
        } else {
            self.records
                .push(HostAudioSettings::new(host_id, volume, muted));
        }
    }

    /// Serializes all host settings into a multi-line string.
    #[must_use]
    pub fn save_to_string(&self) -> String {
        let mut out = String::new();
        for record in &self.records {
            out.push_str(&record.serialize_line());
            out.push('\n');
        }
        out
    }

    /// Parses a multi-line string into a [`HostAudioStore`].
    #[must_use]
    pub fn load_from_str(content: &str) -> Self {
        let mut store = Self::new();
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            if let Some(settings) = HostAudioSettings::parse_line(trimmed) {
                store.records.push(settings);
            }
        }
        store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::audio::{AudioChannels, AudioGeneration};

    #[test]
    fn instant_mute_and_volume_gain() {
        let mut ctrl = AudioVolumeControl::new();
        assert_eq!(ctrl.effective_gain(), 1.0);
        assert!(!ctrl.is_muted());

        // Instant mute toggle
        assert!(ctrl.toggle_mute());
        assert_eq!(ctrl.effective_gain(), 0.0);

        // Instant unmute toggle
        assert!(!ctrl.toggle_mute());
        assert_eq!(ctrl.effective_gain(), 1.0);

        // Change volume
        ctrl.set_volume(0.5);
        assert_eq!(ctrl.effective_gain(), 0.5);

        // Mute overrides volume to 0.0
        ctrl.set_muted(true);
        assert_eq!(ctrl.effective_gain(), 0.0);
    }

    #[test]
    fn apply_volume_to_pcm() {
        let mut ctrl = AudioVolumeControl::new();
        ctrl.set_volume(0.5);

        let generation = AudioGeneration::INITIAL;
        let samples = [1000i16, -2000i16, 3000i16, -4000i16];
        let mut frame =
            AudioPcmFrame::from_interleaved(generation, AudioChannels::Stereo, 0, &samples)
                .unwrap();

        ctrl.apply_to_pcm(&mut frame);
        assert_eq!(frame.samples(), &[500i16, -1000i16, 1500i16, -2000i16]);

        // Mute zeros out all samples
        ctrl.set_muted(true);
        ctrl.apply_to_pcm(&mut frame);
        assert_eq!(frame.samples(), &[0i16, 0i16, 0i16, 0i16]);
    }

    #[test]
    fn per_host_persistence_round_trip() {
        let mut store = HostAudioStore::new();
        let mut host1_ctrl = AudioVolumeControl::new();
        host1_ctrl.set_volume(0.75);
        host1_ctrl.set_muted(true);

        store.save_settings("workstation.tailnet.ts.net", &host1_ctrl);

        let serialized = store.save_to_string();
        let loaded = HostAudioStore::load_from_str(&serialized);

        let recovered = loaded.get_or_default("workstation.tailnet.ts.net");
        assert_eq!(recovered.volume(), 0.75);
        assert!(recovered.is_muted());

        let default_host = loaded.get_or_default("other.tailnet.ts.net");
        assert_eq!(default_host.volume(), 1.0);
        assert!(!default_host.is_muted());
    }
}
