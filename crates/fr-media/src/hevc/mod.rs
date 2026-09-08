//! Bounded, stateful admission of the native HEVC low-delay subset.
//!
//! Checks actual VPS/SPS/PPS and independent slice headers, not container keyframe
//! flags. A configuration generation fixes all three parameter sets byte-for-byte.
//! Initial/recovery IDRs carry all sets; P pictures reference exactly the preceding
//! POC. Admission is transactional: rejected AUs cannot poison subsequent work.
//!
//! This is not a codec, sandbox, or proof of valid CABAC/pixels. Native decoders
//! remain process-isolated and subject to deadlines. This first subset deliberately
//! refuses tiles, dependent/multiple slices, weighted prediction, custom scaling
//! lists, HRD, PCM, long-term/SPS-table references and parameter-set extensions.
//! Broader hardware output must be qualified, not silently accepted.
use crate::config::CodecConfiguration;
use core::fmt;
use fr_core::limits::ProtocolLimits;
use nal::{AnnexB, Nal};
use std::sync::Arc;

mod nal;
mod parameters;
#[cfg(test)]
mod tests;

/// Content-free admission refusal. Error formatting never prints compressed data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HevcError {
    /// Broken start code/length framing.
    Framing,
    /// An incomplete NAL, RBSP or slice header.
    Truncated,
    /// Forbidden bit, nonzero layer, or multiple temporal layers.
    UnsupportedLayer,
    /// Unknown/unsupported syntax is not safe to skip.
    UnsupportedSyntax,
    /// Invalid emulation-prevention byte sequence.
    EmulationPrevention,
    /// A byte, count, geometry or arithmetic bound was exceeded.
    Limit,
    /// The declared decoded-picture budget or zero-reordering rule was exceeded.
    DpbLimit,
    /// The SPS coded/visible geometry disagrees with admission.
    GeometryMismatch,
    /// The SPS color description is missing or contradicts admission.
    ColorMismatch,
    /// Startup or recovery omitted a required VPS/SPS/PPS.
    MissingParameterSet,
    /// Parameter sets changed without a new configuration generation.
    ChangedParameterSet,
    /// A parameter-set/slice identifier refers to an unadmitted set.
    ParameterSetReference,
    /// Multiple pictures, a B picture, or a false IDR declaration.
    PictureMismatch,
    /// A P picture does not depend solely on the immediately previous picture.
    ReferenceMismatch,
    /// Bounded parameter storage could not be reserved.
    Allocation,
}
impl fmt::Display for HevcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HEVC admission refused: {self:?}")
    }
}
impl core::error::Error for HevcError {}

/// Facts read from the admitted bitstream, not requested encoder settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureInfo {
    /// True only for IDR NAL types 19/20 with an I slice.
    pub idr: bool,
    /// Signalled POC LSB (zero for IDR).
    pub poc_lsb: u32,
    /// SPS decoded-picture-buffer bound, independent of compressed reassembly.
    pub decoded_pictures: u8,
    /// Signalled HEVC level (`general_level_idc`).
    pub level_idc: u8,
}
struct ParameterSets {
    bytes: [Vec<u8>; 3],
    sps: parameters::Sps,
    pps: parameters::Pps,
}
/// One stream/configuration's bitstream state. Clone shares bounded immutable
/// parameter storage, allowing a native caller to commit only after codec send
/// succeeds (discard the clone on backpressure/cancellation).
#[derive(Clone)]
pub struct HevcGuard {
    config: CodecConfiguration,
    limits: ProtocolLimits,
    max_decoded_pictures: u8,
    sets: Option<Arc<ParameterSets>>,
    last_poc: Option<u32>,
    available_history: u8,
}
impl fmt::Debug for HevcGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HevcGuard")
            .field("generation", &self.config.generation())
            .field("configured", &self.sets.is_some())
            .field("max_decoded_pictures", &self.max_decoded_pictures)
            .finish_non_exhaustive()
    }
}
impl HevcGuard {
    /// The DPB cap is a decoded-surface budget, NOT the compressed reassembly
    /// window. The current contract admits 2..=12 decoded pictures.
    pub fn new(
        config: CodecConfiguration,
        limits: ProtocolLimits,
        max_decoded_pictures: u8,
    ) -> Result<Self, HevcError> {
        let g = config.geometry();
        limits
            .validate_coded_dimensions(g.coded_width(), g.coded_height())
            .map_err(|_| HevcError::Limit)?;
        if !(2..=12).contains(&max_decoded_pictures) {
            return Err(HevcError::DpbLimit);
        }
        Ok(Self {
            config,
            limits,
            max_decoded_pictures,
            sets: None,
            last_poc: None,
            available_history: 0,
        })
    }
    /// Validates actual Annex B bytes before native submission. Only successful
    /// complete AUs advance this guard. `declared_idr` must match the bitstream.
    pub fn validate_annex_b(
        &mut self,
        bytes: &[u8],
        declared_idr: bool,
    ) -> Result<PictureInfo, HevcError> {
        self.limits
            .validate_access_unit_len(bytes.len())
            .map_err(|_| HevcError::Limit)?;
        let mut units = AnnexB::new(bytes)?;
        let mut sets: [Option<&[u8]>; 3] = [None; 3];
        let mut picture = None;
        let mut aud = false;
        while let Some(nal) = units.next()? {
            match nal.kind {
                32..=34 => {
                    if picture.is_some() || !declared_idr {
                        return Err(HevcError::ChangedParameterSet);
                    }
                    let slot = &mut sets[usize::from(nal.kind - 32)];
                    if slot.is_some() {
                        return Err(HevcError::ChangedParameterSet);
                    }
                    // Bound before retaining even a borrowed parameter-set record.
                    nal.bits()?;
                    *slot = Some(nal.bytes);
                }
                1 | 19 | 20 => {
                    if picture.replace(nal).is_some() {
                        return Err(HevcError::PictureMismatch);
                    }
                }
                35 => {
                    if aud || picture.is_some() || sets.iter().any(Option::is_some) {
                        return Err(HevcError::PictureMismatch);
                    }
                    let mut b = nal.bits()?;
                    b.expect(3, u32::from(!declared_idr))?;
                    b.end()?;
                    aud = true;
                }
                39 | 40 => user_data_sei(nal)?,
                _ => return Err(HevcError::UnsupportedSyntax),
            }
        }
        let picture = picture.ok_or(HevcError::PictureMismatch)?;
        let idr = matches!(picture.kind, 19 | 20);
        if idr != declared_idr {
            return Err(HevcError::PictureMismatch);
        }
        if idr && sets.iter().any(Option::is_none) {
            return Err(HevcError::MissingParameterSet);
        }
        let new_sets = if self.sets.is_none() {
            if !idr {
                return Err(HevcError::MissingParameterSet);
            }
            Some(self.parse_sets(sets)?)
        } else {
            None
        };
        let active = self
            .sets
            .as_deref()
            .or(new_sets.as_ref())
            .ok_or(HevcError::MissingParameterSet)?;
        for (incoming, admitted) in sets.iter().zip(&active.bytes) {
            if incoming.is_some_and(|bytes| bytes != admitted.as_slice()) {
                return Err(HevcError::ChangedParameterSet);
            }
        }
        let poc = parameters::slice(
            picture,
            active.sps,
            active.pps,
            self.config,
            self.available_history,
        )?;
        if !idr
            && self
                .last_poc
                .map(|last| (last + 1) & ((1 << active.sps.poc_bits) - 1))
                != Some(poc)
        {
            return Err(HevcError::ReferenceMismatch);
        }
        let result = PictureInfo {
            idr,
            poc_lsb: poc,
            decoded_pictures: active.sps.dpb,
            level_idc: active.sps.profile.0[11],
        };
        if let Some(sets) = new_sets {
            self.sets = Some(Arc::new(sets));
        }
        self.last_poc = Some(poc);
        self.available_history = if idr {
            1
        } else {
            (self.available_history + 1).min(result.decoded_pictures - 1)
        };
        Ok(result)
    }
    fn parse_sets(&self, sets: [Option<&[u8]>; 3]) -> Result<ParameterSets, HevcError> {
        let [v, s, p] = sets.map(|s| s.ok_or(HevcError::MissingParameterSet));
        let (v, s, p) = (Nal::new(v?)?, Nal::new(s?)?, Nal::new(p?)?);
        let vps = parameters::vps(v, self.max_decoded_pictures)?;
        let sps = parameters::sps(s, vps, self.config, self.limits, self.max_decoded_pictures)?;
        let pps = parameters::pps(p, sps)?;
        let mut bytes = [Vec::new(), Vec::new(), Vec::new()];
        for (dst, nal) in bytes.iter_mut().zip([v, s, p]) {
            dst.try_reserve_exact(nal.bytes.len())
                .map_err(|_| HevcError::Allocation)?;
            dst.extend_from_slice(nal.bytes);
        }
        Ok(ParameterSets { bytes, sps, pps })
    }
}
fn user_data_sei(nal: Nal<'_>) -> Result<(), HevcError> {
    if nal.bytes.len() > 16_384 {
        return Err(HevcError::Limit);
    }
    let mut b = nal.bits()?;
    while !b.at_trailing_bits() {
        // Only inert user_data_unregistered SEI is in the baseline. Do not let
        // timing, active-parameter-set, HDR or orientation messages change it.
        b.expect(8, 5)?;
        let mut size = 0_u32;
        loop {
            let byte = b.read(8)?;
            size += byte;
            if size > 16_384 {
                return Err(HevcError::Limit);
            }
            if byte != 255 {
                break;
            }
        }
        if size < 16 {
            return Err(HevcError::Truncated);
        }
        for _ in 0..size {
            b.read(8)?;
        }
    }
    b.end()
}
