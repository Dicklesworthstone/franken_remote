//! Session-facing native playback shared by the desktop clients.
//! Device calls and destruction belong to the bounded output thread. Opus runs
//! only in the separately supervised child; the session exchanges bounded data.
#![forbid(unsafe_code)]
use std::path::PathBuf;

/// Content-free outcome for the completion record.
#[derive(Debug, Default)]
pub struct Report {
    /// Exact local preference used by the actual output and optional panel.
    pub volume: fr_client::audio::AudioVolumeControl,
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    pub controls: Option<Result<crate::file_progress::Control, crate::file_progress::Error>>,
    /// The host configured a stream and this client acknowledged it.
    pub acknowledged: bool,
    /// Decoded frames the local audio server accepted (not audibility).
    pub submitted: u64,
    /// Valid packets discarded before worker custody because its fixed inbox
    /// was full. Not codec loss, concealment, submitted PCM or a failed epoch.
    pub dropped_packets: u64,
    /// Local output resets, each followed by a fresh host epoch.
    pub resets: u64,
    /// Output generations retired by the original audio freshness deadline.
    pub expired_generations: u64,
    /// Output generations retired for another native/worker failure.
    pub failed_generations: u64,
    /// Prior retired generations followed by acknowledged, submitted fresh PCM.
    pub recovered_generations: u64,
    /// First non-freshness output failure, retained even if playback recovers.
    pub first_failure: Option<&'static str>,
    /// Typed reason audio is absent or ended; a proven replacement can clear it.
    pub absence: Option<&'static str>,
    /// Lifetime submissions at the start of the current output generation.
    pub generation_start: u64,
}
impl Report {
    /// Local preference only; constructing a report never grants playback.
    pub fn new(volume: f32, muted: bool) -> Self {
        Self {
            volume: fr_client::audio::AudioVolumeControl::from_parts(volume, muted),
            ..Self::default()
        }
    }
    /// Retain the latest local preference across reconnects, without allowing
    /// an old panel or output handle to change the next attempt's preference.
    pub fn begin_attempt(&mut self) -> fr_client::audio::AudioVolumeControl {
        let (volume, muted) = self.volume.snapshot();
        self.volume = fr_client::audio::AudioVolumeControl::from_parts(volume, muted);
        #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
        {
            self.controls = None;
        }
        self.acknowledged = false;
        self.generation_start = self.submitted;
        self.volume.clone()
    }
    pub fn active(&self) -> bool {
        self.acknowledged && self.submitted > self.generation_start && self.absence.is_none()
    }
    pub fn absent(&mut self, reason: &'static str) {
        self.absence.get_or_insert(reason);
    }
    #[cfg(any(
        feature = "linux-audio",
        feature = "macos-audio",
        feature = "windows-audio",
        test
    ))]
    pub fn begin_generation(&mut self) {
        self.acknowledged = false;
        self.generation_start = self.submitted;
    }
    #[cfg(any(
        feature = "linux-audio",
        feature = "macos-audio",
        feature = "windows-audio",
        test
    ))]
    pub fn failed_generation(&mut self, reason: &'static str, expired: bool) {
        if expired {
            self.expired_generations = self.expired_generations.saturating_add(1);
        } else {
            self.failed_generations = self.failed_generations.saturating_add(1);
            self.first_failure.get_or_insert(reason);
        }
        self.acknowledged = false;
        self.absence = Some(reason);
    }
    /// Called only for the current, fault-free worker. Configuration, silence,
    /// old-generation submissions and cleanup cannot count as recovery.
    #[cfg(any(
        feature = "linux-audio",
        feature = "macos-audio",
        feature = "windows-audio",
        test
    ))]
    pub fn recovered(&mut self) {
        if self.acknowledged && self.submitted > self.generation_start {
            self.recovered_generations = self
                .expired_generations
                .saturating_add(self.failed_generations);
            self.absence = None;
        }
    }
}

#[cfg(test)]
#[path = "bin/fr_cli/linux/audio/report_tests.rs"]
mod report_tests;

/// Matching trusted decoder sibling; never a peer path or PATH lookup.
pub fn decoder_image() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "fr-opus-worker.exe"
    } else {
        "fr-opus-worker"
    };
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join(name)))
        // Windows validates and pins the protected local image on its admitted
        // supervisor. A network path or blocked filesystem must never cause
        // metadata I/O on the session thread before that boundary.
        .filter(|p| p.is_absolute() && (cfg!(windows) || p.is_file()))
}

// The dependency marker alone may come from another platform's feature.
// Compile a native engine only when this target's actual backend is enabled.
#[cfg(any(
    all(target_os = "linux", feature = "linux-audio"),
    all(target_os = "macos", feature = "macos-audio"),
    all(windows, feature = "windows-audio")
))]
mod native;
#[cfg(any(
    all(target_os = "linux", feature = "linux-audio"),
    all(target_os = "macos", feature = "macos-audio"),
    all(windows, feature = "windows-audio")
))]
pub use native::threaded::{Output, Retirement};
