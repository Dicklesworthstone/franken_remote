//! Bounded client-side HEVC decode admission against untrusted foreign input (Plan §16.1, AGENTS.md §3.2).
#![forbid(unsafe_code)]

use fr_core::limits::ProtocolLimits;

/// Hard upper bound on slices per access unit to prevent CPU/GPU slice header flood attacks.
pub const MAX_SLICES_PER_FRAME: u32 = 64;

/// Maximum admitted decoded picture buffer (DPB) surfaces for client hardware decoders.
pub const MAX_DECODED_PICTURE_BUFFER: u8 = 16;

/// Typed refusal reasons when host-supplied HEVC configuration or frames violate admission limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeAdmissionRefusal {
    DimensionExceeded { width: u32, height: u32, max: u32 },
    UnsupportedProfile { profile_idc: u8 },
    BitDepthMismatch { bit_depth: u8 },
    SliceCountExceeded { count: u32, max: u32 },
    AccessUnitTooLarge { bytes: usize, max: usize },
    InvalidParameterSets,
}

/// Admission gate for client decoders ingesting untrusted foreign host media.
#[derive(Debug, Clone)]
pub struct DecoderAdmissionController {
    limits: ProtocolLimits,
    active_width: u32,
    active_height: u32,
    max_slices: u32,
}

impl DecoderAdmissionController {
    /// Create a new decoder admission controller bound by protocol limits.
    pub fn new(limits: ProtocolLimits) -> Self {
        Self {
            limits,
            active_width: 0,
            active_height: 0,
            max_slices: MAX_SLICES_PER_FRAME,
        }
    }

    /// Admit or refuse a new HEVC decoder configuration before foreign FFI setup.
    pub fn admit_configuration(
        &mut self,
        coded_width: u32,
        coded_height: u32,
        profile_idc: u8,
        bit_depth: u8,
        dpb_size: u8,
    ) -> Result<(), DecodeAdmissionRefusal> {
        self.limits
            .validate_coded_dimensions(coded_width, coded_height)
            .map_err(|_| DecodeAdmissionRefusal::DimensionExceeded {
                width: coded_width,
                height: coded_height,
                max: self.limits.max_dimension_pixels(),
            })?;

        // Baseline HEVC Main profile is profile_idc == 1
        if profile_idc != 1 {
            return Err(DecodeAdmissionRefusal::UnsupportedProfile { profile_idc });
        }

        // Baseline is 8-bit; reject 10/12-bit without true 4:4:4 / range extension negotiation
        if bit_depth != 8 {
            return Err(DecodeAdmissionRefusal::BitDepthMismatch { bit_depth });
        }

        if dpb_size > MAX_DECODED_PICTURE_BUFFER || dpb_size == 0 {
            return Err(DecodeAdmissionRefusal::InvalidParameterSets);
        }

        self.active_width = coded_width;
        self.active_height = coded_height;
        Ok(())
    }

    /// Admit or refuse an encoded access unit chunk before handing to the foreign hardware decoder.
    pub fn admit_access_unit(
        &self,
        byte_length: usize,
        slice_count: u32,
    ) -> Result<(), DecodeAdmissionRefusal> {
        let max_bytes = self.limits.max_encoded_access_unit_bytes() as usize;
        if byte_length > max_bytes {
            return Err(DecodeAdmissionRefusal::AccessUnitTooLarge {
                bytes: byte_length,
                max: max_bytes,
            });
        }
        if slice_count > self.max_slices {
            return Err(DecodeAdmissionRefusal::SliceCountExceeded {
                count: slice_count,
                max: self.max_slices,
            });
        }
        Ok(())
    }

    pub fn active_dimensions(&self) -> Option<(u32, u32)> {
        if self.active_width > 0 && self.active_height > 0 {
            Some((self.active_width, self.active_height))
        } else {
            None
        }
    }
}
