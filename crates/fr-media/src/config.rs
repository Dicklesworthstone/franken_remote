//! The validated HEVC baseline configuration (plan section 8.1).
//!
//! The interoperability baseline is HEVC Main, 8-bit, 4:2:0, SDR, with color
//! primaries, transfer, matrix, and range signalled independently. This module
//! refuses to construct anything outside the admitted subset: no frame
//! reordering, no future-frame lookahead, a simple low-delay I/P reference
//! chain, coded dimensions advertised separately from the visible crop, and a
//! GOP policy that bounds *active* encoding without waking a static screen.
//!
//! HDR is not silently downgraded: an HDR desktop either takes a qualified
//! tone-map into SDR or is a typed refusal ([`ConfigError::HdrNotRepresentable`]),
//! never clipped pixels mislabelled as SDR.

use core::error::Error;
use core::fmt;

use fr_core::ids::CodecConfigurationGeneration;
use fr_core::limits::{LimitsError, ProtocolLimits};

/// HEVC profile. Only the Main 8-bit 4:2:0 baseline is constructible here;
/// richer profiles are optional extensions negotiated separately and are not
/// part of this baseline contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum CodecProfile {
    /// HEVC Main, 8-bit, 4:2:0 — the interoperability baseline.
    Main8_420,
}

/// Color range (plan section 8.1: limited/full mismatches are a real defect).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorRange {
    /// Studio/limited range.
    Limited,
    /// Full range.
    Full,
}

/// Transfer characteristics. SDR is the default; the HDR variants exist so a
/// source can be *recognised* as HDR and routed to tone-mapping or refusal —
/// never encoded as if it were SDR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TransferFunction {
    /// BT.709 SDR.
    Bt709,
    /// sRGB SDR.
    Srgb,
    /// PQ (SMPTE ST 2084) HDR.
    Pq,
    /// HLG HDR.
    Hlg,
}

impl TransferFunction {
    /// True for the HDR transfer functions.
    #[must_use]
    pub const fn is_hdr(self) -> bool {
        matches!(self, Self::Pq | Self::Hlg)
    }
}

/// Color primaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColorPrimaries {
    /// BT.709.
    Bt709,
    /// BT.2020.
    Bt2020,
}

/// Color matrix coefficients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColorMatrix {
    /// BT.709.
    Bt709,
    /// BT.2020 non-constant luminance.
    Bt2020Ncl,
}

/// Independently signalled color information (plan section 8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ColorInfo {
    /// Color primaries.
    pub primaries: ColorPrimaries,
    /// Transfer function.
    pub transfer: TransferFunction,
    /// Matrix coefficients.
    pub matrix: ColorMatrix,
    /// Range.
    pub range: ColorRange,
}

impl ColorInfo {
    /// The SDR default: BT.709 primaries/transfer/matrix, limited range.
    #[must_use]
    pub const fn sdr_bt709() -> Self {
        Self {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            matrix: ColorMatrix::Bt709,
            range: ColorRange::Limited,
        }
    }
}

/// Coded dimensions advertised separately from the visible crop (plan section
/// 8.1). Coded dimensions are rounded up to the backend's alignment; the crop
/// is what the client presents. Padding between them must be initialised by
/// the backend — this type records the distinction so it cannot be lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodedGeometry {
    coded_width: u32,
    coded_height: u32,
    crop_width: u32,
    crop_height: u32,
}

impl CodedGeometry {
    /// Builds coded+crop geometry, validating that the crop fits within the
    /// coded dimensions, the coded dimensions are aligned to `alignment` (a
    /// nonzero power of two), and both are within the negotiated limits.
    pub fn new(
        limits: &ProtocolLimits,
        coded_width: u32,
        coded_height: u32,
        crop_width: u32,
        crop_height: u32,
        alignment: u32,
    ) -> Result<Self, ConfigError> {
        limits.validate_coded_dimensions(coded_width, coded_height)?;
        if crop_width == 0 || crop_height == 0 {
            return Err(ConfigError::Limits(LimitsError::ZeroDimension));
        }
        if alignment == 0 || !alignment.is_power_of_two() {
            return Err(ConfigError::UnalignedCoded {
                dimension: alignment,
            });
        }
        if !coded_width.is_multiple_of(alignment) || !coded_height.is_multiple_of(alignment) {
            return Err(ConfigError::UnalignedCoded {
                dimension: if coded_width.is_multiple_of(alignment) {
                    coded_height
                } else {
                    coded_width
                },
            });
        }
        if crop_width > coded_width || crop_height > coded_height {
            return Err(ConfigError::CropExceedsCoded {
                crop_width,
                crop_height,
                coded_width,
                coded_height,
            });
        }
        Ok(Self {
            coded_width,
            coded_height,
            crop_width,
            crop_height,
        })
    }

    /// Coded (allocation) width.
    #[must_use]
    pub const fn coded_width(&self) -> u32 {
        self.coded_width
    }
    /// Coded (allocation) height.
    #[must_use]
    pub const fn coded_height(&self) -> u32 {
        self.coded_height
    }
    /// Visible (presented) width.
    #[must_use]
    pub const fn crop_width(&self) -> u32 {
        self.crop_width
    }
    /// Visible (presented) height.
    #[must_use]
    pub const fn crop_height(&self) -> u32 {
        self.crop_height
    }
    /// True when coded and crop dimensions differ, so the backend MUST have
    /// initialised the padding region (plan section 8.1, 11.4).
    #[must_use]
    pub const fn has_padding(&self) -> bool {
        self.coded_width != self.crop_width || self.coded_height != self.crop_height
    }
}

/// The low-delay reference/GOP policy for the baseline (plan section 8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GopPolicy {
    max_gop_frames: u32,
}

impl GopPolicy {
    /// The provisional baseline: a maximum GOP of two seconds of *active*
    /// encoding at the given frame rate. This never means waking a static
    /// screen every two seconds — it bounds the reference chain only while
    /// frames are actually being produced (plan section 8.1).
    pub fn baseline_for_frame_rate(frames_per_second: u32) -> Result<Self, ConfigError> {
        if frames_per_second == 0 {
            return Err(ConfigError::ZeroFrameRate);
        }
        // Two seconds of active encoding.
        let max_gop_frames = frames_per_second
            .checked_mul(2)
            .ok_or(ConfigError::GopOverflow)?;
        Ok(Self { max_gop_frames })
    }

    /// The maximum number of frames between IDRs during active encoding.
    #[must_use]
    pub const fn max_gop_frames(&self) -> u32 {
        self.max_gop_frames
    }
}

/// A complete, validated HEVC baseline configuration. Carries the
/// configuration generation so that a decoder reconfiguration is fenced
/// against stale access units (plan section 12.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecConfiguration {
    generation: CodecConfigurationGeneration,
    profile: CodecProfile,
    geometry: CodedGeometry,
    color: ColorInfo,
    gop: GopPolicy,
}

impl CodecConfiguration {
    /// Builds a baseline configuration. Refuses HDR transfer functions: an HDR
    /// source must be tone-mapped to an SDR [`ColorInfo`] by the caller before
    /// reaching here, or reported as unrepresentable — this constructor never
    /// silently encodes HDR as SDR (plan section 8.1).
    pub fn new_baseline(
        generation: CodecConfigurationGeneration,
        geometry: CodedGeometry,
        color: ColorInfo,
        gop: GopPolicy,
    ) -> Result<Self, ConfigError> {
        if color.transfer.is_hdr() {
            return Err(ConfigError::HdrNotRepresentable {
                transfer: color.transfer,
            });
        }
        Ok(Self {
            generation,
            profile: CodecProfile::Main8_420,
            geometry,
            color,
            gop,
        })
    }

    /// The configuration generation.
    #[must_use]
    pub const fn generation(&self) -> CodecConfigurationGeneration {
        self.generation
    }
    /// The profile (always the baseline in this contract).
    #[must_use]
    pub const fn profile(&self) -> CodecProfile {
        self.profile
    }
    /// The coded+crop geometry.
    #[must_use]
    pub const fn geometry(&self) -> CodedGeometry {
        self.geometry
    }
    /// The color signalling.
    #[must_use]
    pub const fn color(&self) -> ColorInfo {
        self.color
    }
    /// The GOP policy.
    #[must_use]
    pub const fn gop(&self) -> GopPolicy {
        self.gop
    }
}

/// Why a configuration could not be constructed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ConfigError {
    /// A shared-limits violation (dimensions, pixels).
    Limits(LimitsError),
    /// Coded dimensions are not aligned to the required power-of-two boundary.
    UnalignedCoded {
        /// The offending dimension (or the bad alignment value).
        dimension: u32,
    },
    /// The visible crop exceeds the coded dimensions.
    CropExceedsCoded {
        /// Requested crop width.
        crop_width: u32,
        /// Requested crop height.
        crop_height: u32,
        /// Coded width.
        coded_width: u32,
        /// Coded height.
        coded_height: u32,
    },
    /// An HDR transfer function reached the SDR baseline constructor.
    HdrNotRepresentable {
        /// The HDR transfer function that was refused.
        transfer: TransferFunction,
    },
    /// A zero frame rate was supplied to the GOP policy.
    ZeroFrameRate,
    /// The GOP frame count overflowed.
    GopOverflow,
}

impl From<LimitsError> for ConfigError {
    fn from(e: LimitsError) -> Self {
        Self::Limits(e)
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Limits(e) => write!(f, "limits: {e}"),
            Self::UnalignedCoded { dimension } => {
                write!(f, "coded dimension {dimension} is not aligned")
            }
            Self::CropExceedsCoded {
                crop_width,
                crop_height,
                coded_width,
                coded_height,
            } => write!(
                f,
                "crop {crop_width}x{crop_height} exceeds coded {coded_width}x{coded_height}"
            ),
            Self::HdrNotRepresentable { transfer } => {
                write!(
                    f,
                    "HDR transfer {transfer:?} cannot be represented as SDR baseline"
                )
            }
            Self::ZeroFrameRate => f.write_str("zero frame rate"),
            Self::GopOverflow => f.write_str("GOP frame count overflow"),
        }
    }
}

impl Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen0() -> CodecConfigurationGeneration {
        CodecConfigurationGeneration::INITIAL
    }

    #[test]
    fn coded_geometry_validates_alignment_and_crop() {
        let l = ProtocolLimits::ABSOLUTE;
        // 1920x1080 cropped inside 1920x1088 (16-aligned) is valid.
        let g = CodedGeometry::new(&l, 1920, 1088, 1920, 1080, 16).unwrap();
        assert!(g.has_padding());
        assert_eq!(g.crop_height(), 1080);
        assert_eq!(g.coded_height(), 1088);

        // Non-aligned coded height refuses.
        assert!(matches!(
            CodedGeometry::new(&l, 1920, 1080, 1920, 1080, 16),
            Err(ConfigError::UnalignedCoded { .. })
        ));
        // Crop larger than coded refuses.
        assert!(matches!(
            CodedGeometry::new(&l, 1920, 1088, 1920, 1089, 16),
            Err(ConfigError::CropExceedsCoded { .. })
        ));
        // Over-limit coded dimension refuses via shared limits.
        assert!(matches!(
            CodedGeometry::new(&l, 8208, 16, 8208, 16, 16),
            Err(ConfigError::Limits(_))
        ));
    }

    #[test]
    fn baseline_config_refuses_hdr_but_accepts_sdr() {
        let l = ProtocolLimits::ABSOLUTE;
        let geom = CodedGeometry::new(&l, 1920, 1088, 1920, 1080, 16).unwrap();
        let gop = GopPolicy::baseline_for_frame_rate(60).unwrap();
        assert_eq!(gop.max_gop_frames(), 120);

        let ok = CodecConfiguration::new_baseline(gen0(), geom, ColorInfo::sdr_bt709(), gop);
        assert!(ok.is_ok());

        let hdr = ColorInfo {
            transfer: TransferFunction::Pq,
            primaries: ColorPrimaries::Bt2020,
            matrix: ColorMatrix::Bt2020Ncl,
            range: ColorRange::Limited,
        };
        assert!(matches!(
            CodecConfiguration::new_baseline(gen0(), geom, hdr, gop),
            Err(ConfigError::HdrNotRepresentable {
                transfer: TransferFunction::Pq
            })
        ));
    }

    #[test]
    fn gop_policy_rejects_zero_frame_rate() {
        assert_eq!(
            GopPolicy::baseline_for_frame_rate(0),
            Err(ConfigError::ZeroFrameRate)
        );
    }

    #[test]
    fn hdr_detection_is_correct() {
        assert!(TransferFunction::Pq.is_hdr());
        assert!(TransferFunction::Hlg.is_hdr());
        assert!(!TransferFunction::Bt709.is_hdr());
        assert!(!TransferFunction::Srgb.is_hdr());
    }
}
