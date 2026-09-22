//! User-configurable client settings for desktop shells (Plan §§14.1, 15.1, 16.1).
#![forbid(unsafe_code)]

use crate::shortcut::ShortcutCaptureMode;
use serde::{Deserialize, Serialize};

/// Display presentation fit mode for local windowing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DisplayFitMode {
    /// Full remote image is aspect-fitted into local window with black borders.
    #[default]
    AspectFit,
    /// 1:1 remote pixel to local physical display pixel mapping without scaling.
    NativePixels,
}

/// Workstation color fidelity settings (Plan §14.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ColorRangePreference {
    /// Accurate color reproduction matching host CICP metadata (studio vs full).
    #[default]
    Accurate,
    /// Direct compositor passthrough with standard sRGB conversion.
    StandardRgb,
}

/// Desktop client persistent settings model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSettings {
    /// Display presentation mode.
    pub fit_mode: DisplayFitMode,
    /// Color range and metadata handling.
    pub color_preference: ColorRangePreference,
    /// Settle-to-sharp refinement: higher-quality HEVC encode when motion settles (Plan §14.1).
    pub settle_to_sharp: bool,
    /// Remote playback volume (0..=100).
    pub audio_volume: u8,
    /// Remote playback mute state.
    pub audio_muted: bool,
    /// Explicit client microphone transmission consent. Default false (§3.3).
    pub mic_enabled: bool,
    /// System shortcut routing preference.
    pub shortcut_mode: ShortcutCaptureMode,
}

impl Default for ClientSettings {
    fn default() -> Self {
        Self {
            fit_mode: DisplayFitMode::AspectFit,
            color_preference: ColorRangePreference::Accurate,
            settle_to_sharp: true,
            audio_volume: 80,
            audio_muted: false,
            mic_enabled: false,
            shortcut_mode: ShortcutCaptureMode::Disabled,
        }
    }
}

impl ClientSettings {
    /// Clamp audio volume to valid 0..=100 range and validate invariants.
    pub fn sanitize(&mut self) {
        self.audio_volume = self.audio_volume.min(100);
    }
}
