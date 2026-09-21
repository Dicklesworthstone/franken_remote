#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::float_cmp
)]
//! Client-side volume control and instant mute for remote playback audio (Plan §15.4).
//!
//! # Invariants:
//! - Local-only control: volume adjustments and instant mute never require a host round trip.
//! - Saturating arithmetic prevents audio clipping or wrap-around distortion.
//! - Settings can be persisted per remote host identifier.

use fr_media::audio::AudioPcmFrame;

/// Default initial volume level (100%).
pub const DEFAULT_VOLUME: f32 = 1.0;

/// Local client audio volume and mute controller.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AudioVolumeControl {
    volume: f32,
    muted: bool,
}

impl Default for AudioVolumeControl {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioVolumeControl {
    /// Creates a volume controller initialized to unmuted 100% volume.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            volume: DEFAULT_VOLUME,
            muted: false,
        }
    }

    /// Creates a controller with specific volume and mute states.
    #[must_use]
    pub fn from_parts(volume: f32, muted: bool) -> Self {
        Self {
            volume: volume.clamp(0.0, 1.0),
            muted,
        }
    }

    /// Returns current nominal volume level in `[0.0, 1.0]`.
    #[must_use]
    pub const fn volume(&self) -> f32 {
        self.volume
    }

    /// Sets the volume level, clamped to `[0.0, 1.0]`.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    /// Returns whether audio is currently muted.
    #[must_use]
    pub const fn is_muted(&self) -> bool {
        self.muted
    }

    /// Sets mute state directly.
    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Toggles mute state instantly with 0 host round trips.
    ///
    /// Returns the new mute state.
    pub fn toggle_mute(&mut self) -> bool {
        self.muted = !self.muted;
        self.muted
    }

    /// Returns the effective linear multiplier in `[0.0, 1.0]`.
    ///
    /// If muted, returns `0.0`. Otherwise returns `self.volume`.
    #[must_use]
    pub fn effective_gain(&self) -> f32 {
        if self.muted {
            0.0
        } else {
            self.volume
        }
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
        format!("{}={:.4},{}", self.host_id, self.volume, i32::from(self.muted))
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
        Self { records: Vec::new() }
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
        if let Some(entry) = self.records.iter_mut().find(|r| r.host_id == host_id) {
            entry.volume = control.volume();
            entry.muted = control.is_muted();
        } else {
            self.records.push(HostAudioSettings::new(
                host_id,
                control.volume(),
                control.is_muted(),
            ));
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
        let mut frame = AudioPcmFrame::from_interleaved(generation, AudioChannels::Stereo, 0, &samples).unwrap();

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
