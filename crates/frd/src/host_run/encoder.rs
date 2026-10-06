//! Explicit local encoder selection for the existing supervised HEVC worker.
//! Selecting a backend is NOT a successful probe, a qualification result or a
//! reason to bypass native startup/bitstream checks. No automatic substitution
//! exists: a failed hardware attempt stays failed, including on share restart.
use fr_media::worker::Backend;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    SoftwareExplicit,
    Nvenc,
    Vaapi,
}
impl Encoder {
    /// Exact local CLI names, not peer input or an arbitrary `FFmpeg` codec name.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "software" => Some(Self::SoftwareExplicit),
            "nvenc" => Some(Self::Nvenc),
            "vaapi" => Some(Self::Vaapi),
            _ => None,
        }
    }
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SoftwareExplicit => "software",
            Self::Nvenc => "nvenc",
            Self::Vaapi => "vaapi",
        }
    }
    pub const fn is_hardware(self) -> bool {
        !matches!(self, Self::SoftwareExplicit)
    }
    /// The SAME value configures independent observation and controlled capture.
    pub(crate) const fn backend(self) -> Backend {
        match self {
            Self::SoftwareExplicit => Backend::SoftwareExplicit,
            Self::Nvenc => Backend::Nvenc,
            Self::Vaapi => Backend::Vaapi,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_run::choose;
    use fr_core::{ids::DisplayGeometryGeneration, limits::ProtocolLimits};
    use fr_wire::display::{Catalog, Display};

    const ENCODERS: [Encoder; 3] = [Encoder::SoftwareExplicit, Encoder::Nvenc, Encoder::Vaapi];

    fn display(handle: u128, x: i32) -> Display {
        Display {
            handle,
            geometry: DisplayGeometryGeneration::INITIAL,
            x,
            y: 0,
            pixel_width: 1920,
            pixel_height: 1080,
            logical_width: 1920,
            logical_height: 1080,
            scale_numerator: 1,
            scale_denominator: 1,
            rotation: 0,
        }
    }

    #[test]
    fn only_exact_supported_local_backend_names_are_accepted() {
        for encoder in ENCODERS {
            assert_eq!(Encoder::parse(encoder.as_str()), Some(encoder));
        }
        for value in ["", "auto", "hardware", "h264", "libx265", "videotoolbox", "NVENC", " vaapi", "vaapi ", "nvenc,software"] {
            assert_eq!(Encoder::parse(value), None, "{value}");
        }
        assert!(!Encoder::SoftwareExplicit.is_hardware());
        assert!(Encoder::Nvenc.is_hardware());
        assert!(Encoder::Vaapi.is_hardware());
    }

    #[test]
    fn chosen_backend_survives_display_selection_and_private_worker_encoding() {
        let catalog = Catalog::new(7, &[display(11, 0)], &ProtocolLimits::ABSOLUTE).unwrap();
        for (encoder, expected) in ENCODERS.into_iter().zip([Backend::SoftwareExplicit, Backend::Nvenc, Backend::Vaapi]) {
            let (selected, configuration) = choose(&catalog, 30, 8_000_000, encoder).unwrap();
            assert_eq!(selected, catalog.selection(11).unwrap());
            assert_eq!(configuration.backend, expected);
            assert_eq!(configuration.backend, encoder.backend());
            assert_eq!(configuration.width, 1920);
            assert_eq!(configuration.height, 1080);
            assert_eq!(configuration.fps, 30);
            assert_eq!(configuration.bitrate, 8_000_000);
            let bytes = configuration.encode().unwrap();
            assert_eq!(fr_media::worker::Configuration::decode(&bytes).unwrap(), configuration);
        }
    }

    #[test]
    fn backend_choice_never_changes_display_preference_or_empty_catalog_refusal() {
        let catalog = Catalog::new(1, &[display(11, -1920), display(22, 0)], &ProtocolLimits::ABSOLUTE).unwrap();
        let empty = Catalog::new(2, &[], &ProtocolLimits::ABSOLUTE).unwrap();
        for encoder in ENCODERS {
            assert_eq!(choose(&catalog, 60, 12_000_000, encoder).unwrap().0, catalog.selection(22).unwrap());
            assert!(choose(&empty, 30, 8_000_000, encoder).is_err());
        }
    }

    #[test]
    fn selection_cannot_bypass_existing_worker_configuration_bounds() {
        let catalog = Catalog::new(1, &[display(11, 0)], &ProtocolLimits::ABSOLUTE).unwrap();
        for encoder in ENCODERS {
            for (fps, bitrate) in [(0, 8_000_000), (241, 8_000_000), (30, 1)] {
                let (_, configuration) = choose(&catalog, fps, bitrate, encoder).unwrap();
                assert_eq!(configuration.backend, encoder.backend());
                assert!(configuration.encode().is_err());
            }
        }
    }
}
