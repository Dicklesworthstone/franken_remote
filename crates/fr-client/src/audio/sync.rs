#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_lossless,
    clippy::unnecessary_cast
)]
//! Bounded audio/video offset alignment and synchronization policy (Plan §15.4).
//!
//! # Core Invariants:
//! - Fresh video is **never** held behind delayed audio; video presentation proceeds unblocked.
//! - Audio that lags video past the permissible skew window is dropped/skipped to catch up.
//! - Audio that leads video past the permissible skew window pauses playout until video arrives.
//! - Generational resets immediately clear synchronization history.

use fr_core::audio::OPUS_SAMPLE_RATE;

/// Maximum allowable audio lag behind video before audio frames are dropped (100 ms).
pub const MAX_AUDIO_LAG_MS: i64 = 100;

/// Maximum allowable audio lead ahead of video before audio playout pauses (120 ms).
pub const MAX_AUDIO_LEAD_MS: i64 = 120;

/// Alignment decision returned by [`AudioVideoSyncController`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AvAlignment {
    /// Audio and video are within permissible skew limits.
    Synchronized { skew_ms: i64 },
    /// Audio is lagging behind video; caller should drop `drop_samples` to catch up.
    AudioLagging { lag_ms: u32, drop_samples: u32 },
    /// Audio is leading video; caller should pause audio playout.
    AudioLeading { lead_ms: u32 },
    /// Video or audio clock has not provided an initial anchor.
    Unanchored,
}

/// Controller governing audio/video timing alignment without delaying video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioVideoSyncController {
    last_video_timestamp_ms: Option<u64>,
    last_audio_timestamp_samples: Option<u64>,
    max_lag_ms: i64,
    max_lead_ms: i64,
}

impl Default for AudioVideoSyncController {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioVideoSyncController {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_video_timestamp_ms: None,
            last_audio_timestamp_samples: None,
            max_lag_ms: MAX_AUDIO_LAG_MS,
            max_lead_ms: MAX_AUDIO_LEAD_MS,
        }
    }

    /// Resets the alignment controller on device change, reconnect, or stream reset.
    pub fn reset(&mut self) {
        self.last_video_timestamp_ms = None;
        self.last_audio_timestamp_samples = None;
    }

    /// Informs the controller of a newly presented video frame's host-monotonic timestamp in milliseconds.
    pub fn update_video_presentation(&mut self, video_timestamp_ms: u64) {
        self.last_video_timestamp_ms = Some(video_timestamp_ms);
    }

    /// Evaluates alignment for an upcoming audio packet with sample timestamp `audio_timestamp_samples`.
    ///
    /// Note: Video presentation is never delayed or modified by this call.
    pub fn check_alignment(&mut self, audio_timestamp_samples: u64) -> AvAlignment {
        self.last_audio_timestamp_samples = Some(audio_timestamp_samples);

        let Some(video_ms) = self.last_video_timestamp_ms else {
            return AvAlignment::Unanchored;
        };

        // Convert audio sample timestamp to milliseconds
        let audio_ms = (audio_timestamp_samples * 1000) / (OPUS_SAMPLE_RATE as u64);

        // Skew = Audio time - Video time
        // Positive skew means audio is ahead (leading)
        // Negative skew means audio is behind (lagging)
        let skew_ms = (audio_ms as i64) - (video_ms as i64);

        if skew_ms < -self.max_lag_ms {
            let lag_ms = (-skew_ms) as u32;
            let drop_samples = (lag_ms * (OPUS_SAMPLE_RATE / 1000)) as u32;
            AvAlignment::AudioLagging {
                lag_ms,
                drop_samples,
            }
        } else if skew_ms > self.max_lead_ms {
            let lead_ms = skew_ms as u32;
            AvAlignment::AudioLeading { lead_ms }
        } else {
            AvAlignment::Synchronized { skew_ms }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unanchored_until_video_arrives() {
        let mut sync = AudioVideoSyncController::new();
        assert_eq!(sync.check_alignment(48_000), AvAlignment::Unanchored);

        sync.update_video_presentation(1000);
        // 48,000 samples at 48 kHz = 1000 ms -> perfectly aligned (skew 0)
        assert_eq!(
            sync.check_alignment(48_000),
            AvAlignment::Synchronized { skew_ms: 0 }
        );
    }

    #[test]
    fn lagging_audio_triggers_drop_without_delaying_video() {
        let mut sync = AudioVideoSyncController::new();
        sync.update_video_presentation(1000); // 1000 ms

        // Audio at 800 ms (lag of 200 ms > 100 ms max lag)
        let audio_samples = 800 * 48; // 38,400 samples
        match sync.check_alignment(audio_samples) {
            AvAlignment::AudioLagging {
                lag_ms,
                drop_samples,
            } => {
                assert_eq!(lag_ms, 200);
                assert_eq!(drop_samples, 200 * 48);
            }
            other => panic!("expected AudioLagging, got {other:?}"),
        }
    }

    #[test]
    fn leading_audio_triggers_pause() {
        let mut sync = AudioVideoSyncController::new();
        sync.update_video_presentation(1000); // 1000 ms

        // Audio at 1200 ms (lead of 200 ms > 120 ms max lead)
        let audio_samples = 1200 * 48; // 57,600 samples
        match sync.check_alignment(audio_samples) {
            AvAlignment::AudioLeading { lead_ms } => {
                assert_eq!(lead_ms, 200);
            }
            other => panic!("expected AudioLeading, got {other:?}"),
        }
    }

    #[test]
    fn synchronized_within_tolerance() {
        let mut sync = AudioVideoSyncController::new();
        sync.update_video_presentation(1000);

        // Audio at 1020 ms (+20 ms lead, within 120 ms)
        let audio_samples = 1020 * 48;
        assert_eq!(
            sync.check_alignment(audio_samples),
            AvAlignment::Synchronized { skew_ms: 20 }
        );

        // Audio at 960 ms (-40 ms lag, within 100 ms)
        let audio_samples = 960 * 48;
        assert_eq!(
            sync.check_alignment(audio_samples),
            AvAlignment::Synchronized { skew_ms: -40 }
        );
    }
}
