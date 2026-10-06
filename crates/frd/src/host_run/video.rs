//! One explicit local video-rate policy for the encoder and capture scheduler.
//! These are admission/codec targets, never measured FPS, bandwidth estimates
//! or authority lifetimes. Native backpressure and source verification remain
//! in force, and no missed capture opportunity is queued for catch-up.
use std::time::Duration;

pub const DEFAULT_FPS: u16 = 30;
pub const DEFAULT_BITRATE: u32 = 8_000_000;
pub const MAX_FPS: u16 = 240;
pub const MIN_BITRATE: u32 = 10_000;
pub const MAX_BITRATE: u32 = 200_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    FrameRate,
    Bitrate,
}

/// Immutable settings, validated against the existing private worker profile.
/// No peer value, device choice or change to observation/control authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    fps: u16,
    bitrate: u32,
}
impl Profile {
    pub fn new(fps: u16, bitrate: u32) -> Result<Self, Error> {
        if !(1..=MAX_FPS).contains(&fps) {
            return Err(Error::FrameRate);
        }
        if !(MIN_BITRATE..=MAX_BITRATE).contains(&bitrate) {
            return Err(Error::Bitrate);
        }
        Ok(Self { fps, bitrate })
    }
    pub const fn fps(self) -> u16 {
        self.fps
    }
    /// Encoder target, not a hard per-packet or instantaneous network ceiling.
    pub const fn bitrate(self) -> u32 {
        self.bitrate
    }
    pub fn capture_interval(self) -> Duration {
        // Round UP to the source clock's microsecond resolution. The previous
        // unconditional 50 ms floor silently capped every encoder at 20 fps.
        Duration::from_micros(1_000_000_u64.div_ceil(u64::from(self.fps)))
    }
}
impl Default for Profile {
    fn default() -> Self {
        Self { fps: DEFAULT_FPS, bitrate: DEFAULT_BITRATE }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_run::{Encoder, capture_interval, choose};
    use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
    use fr_wire::display::{Catalog, Display};

    #[test]
    fn all_supported_rates_use_a_ceiling_not_an_artificial_twenty_fps_cap() {
        for fps in 1..=MAX_FPS {
            let profile = Profile::new(fps, DEFAULT_BITRATE).unwrap();
            let micros = profile.capture_interval().as_micros();
            assert!(micros * u128::from(fps) >= 1_000_000);
            assert!((micros - 1) * u128::from(fps) < 1_000_000);
            assert_eq!(capture_interval(fps), Some(profile.capture_interval()));
        }
        assert_eq!(capture_interval(60), Some(Duration::from_micros(16_667)));
        assert_eq!(capture_interval(120), Some(Duration::from_micros(8_334)));
        assert_eq!(capture_interval(240), Some(Duration::from_micros(4_167)));
        assert_eq!(capture_interval(0), None);
        assert_eq!(capture_interval(MAX_FPS + 1), None);
    }

    #[test]
    fn invalid_rates_refuse_instead_of_being_clamped_or_using_defaults() {
        for fps in [0, MAX_FPS + 1, u16::MAX] {
            assert_eq!(Profile::new(fps, DEFAULT_BITRATE), Err(Error::FrameRate));
        }
        for bitrate in [0, MIN_BITRATE - 1, MAX_BITRATE + 1, u32::MAX] {
            assert_eq!(Profile::new(DEFAULT_FPS, bitrate), Err(Error::Bitrate));
        }
        assert_eq!(Profile::default(), Profile::new(DEFAULT_FPS, DEFAULT_BITRATE).unwrap());
    }

    #[test]
    fn selected_rates_reach_the_real_display_and_private_codec_configuration() {
        let display = Display {
            handle: 17,
            geometry: DisplayGeometryGeneration::INITIAL,
            x: 0, y: 0,
            pixel_width: 1920, pixel_height: 1080,
            logical_width: 1920, logical_height: 1080,
            scale_numerator: 1, scale_denominator: 1, rotation: 0,
        };
        let catalog = Catalog::new(1, &[display], &ProtocolLimits::ABSOLUTE).unwrap();
        for encoder in [Encoder::SoftwareExplicit, Encoder::Nvenc, Encoder::Vaapi] {
            for (fps, bitrate) in [(1, MIN_BITRATE), (60, 12_000_000), (MAX_FPS, MAX_BITRATE)] {
                let profile = Profile::new(fps, bitrate).unwrap();
                let (selection, codec) = choose(&catalog, profile.fps(), profile.bitrate(), encoder).unwrap();
                assert_eq!(selection, catalog.selection(17).unwrap());
                assert_eq!(codec.fps, fps);
                assert_eq!(codec.bitrate, bitrate);
                assert_eq!(codec.backend, encoder.backend());
                let decoded = fr_media::worker::Configuration::decode(&codec.encode().unwrap()).unwrap();
                assert_eq!(decoded, codec);
                assert!(codec.codec().is_ok());
            }
        }
    }
}
