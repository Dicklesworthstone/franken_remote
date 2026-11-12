//! Admission subset of H.265 7.3.2 and 7.3.6, not a video decoder.
//! Syntax cross-checked with `FFmpeg` n7.1.1 `cbs_h265_syntax_template.c`.
//! Unsupported extensions fail closed instead of skipping unknown syntax.
use super::{
    HevcError,
    nal::{Bits, Nal},
};
use crate::config::{
    CodecConfiguration, ColorMatrix, ColorPrimaries, ColorRange, TransferFunction,
};
use fr_core::limits::ProtocolLimits;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Profile(pub [u8; 12]);
/// `max_sub_layers_minus1` and the temporal-ID nesting flag. An encoder may
/// declare temporal sub-layers it never uses: `VideoToolbox`'s low-latency
/// HEVC declares two and puts every picture in the base layer. Declaring them
/// grants nothing. `Nal::new` refuses every NAL whose temporal ID is not zero,
/// an upper sub-layer may signal no profile, level or HRD of its own, and every
/// signalled ordering set obeys the same limits.
fn sub_layers(b: &mut Bits<'_>) -> Result<u32, HevcError> {
    let minus1 = b.read(3)?;
    b.expect(1, 1)?; // Nested, as a single layer must be.
    if minus1 > 6 {
        return Err(HevcError::UnsupportedSyntax); // Seven is reserved.
    }
    Ok(minus1)
}
fn profile(b: &mut Bits<'_>, sub_layers_minus1: u32) -> Result<Profile, HevcError> {
    let mut bytes = [0; 12];
    for byte in &mut bytes {
        *byte = u8::try_from(b.read(8)?).map_err(|_| HevcError::Limit)?;
    }
    // Main profile, default profile space. No interlaced-source claim.
    if bytes[0] & 0xdf != 1 || bytes[5] & 0x40 != 0 || bytes[11] == 0 {
        return Err(HevcError::UnsupportedSyntax);
    }
    // Two clear presence flags per upper sub-layer, then reserved zero pairs
    // up to eight: sixteen zero bits whenever a sub-layer is declared.
    if sub_layers_minus1 > 0 {
        b.expect(16, 0)?;
    }
    Ok(Profile(bytes))
}
/// Every signalled sub-layer set must fit the budget and never reorder; the
/// largest governs decoding.
fn ordering(b: &mut Bits<'_>, max_dpb: u8, sub_layers_minus1: u32) -> Result<u8, HevcError> {
    let sets = if b.flag()? { sub_layers_minus1 + 1 } else { 1 };
    let mut largest = 0;
    for _ in 0..sets {
        let dpb = b.ue(15)? + 1;
        if dpb > u32::from(max_dpb) || b.ue(15)? != 0 {
            return Err(HevcError::DpbLimit);
        }
        b.ue(u32::MAX - 1)?;
        largest = largest.max(dpb);
    }
    u8::try_from(largest).map_err(|_| HevcError::DpbLimit)
}
/// At most this many SPS short-term reference-picture sets are admitted.
const MAX_SPS_RPS: usize = 4;
const MAX_SPS_RPS_COUNT: u32 = 4;
/// One short-term reference-picture set reduced to what admission checks:
/// past pictures only (no reordering), the first being the previous picture.
#[derive(Clone, Copy, Default)]
pub(super) struct Rps {
    past: u32,
    max_distance: u32,
    first_is_previous: bool,
    /// Entries used by the current picture (`NumPicTotalCurr`).
    used: u32,
}
impl Rps {
    /// `st_ref_pic_set` without inter-RPS prediction (the caller has consumed
    /// or ruled out that flag). Future (positive) pictures are refused.
    fn parse(b: &mut Bits<'_>, dpb: u8) -> Result<Self, HevcError> {
        let past = b.ue(u32::from(dpb) - 1)?;
        if b.ue(15)? != 0 {
            return Err(HevcError::ReferenceMismatch);
        }
        let mut set = Self {
            past,
            ..Self::default()
        };
        let mut distance = 0;
        for index in 0..past {
            distance += b.ue(32767)? + 1;
            let used = b.flag()?;
            set.used += u32::from(used);
            if index == 0 {
                set.first_is_previous = distance == 1 && used;
            }
            set.max_distance = distance;
        }
        Ok(set)
    }
    /// Every entry is a past picture still in the history; the first is the
    /// previous picture. At most two are active per list (PPS and slice
    /// active counts) and no slice reorders its lists.
    fn check(self, available_history: u8) -> Result<(), HevcError> {
        if self.past == 0
            || !self.first_is_previous
            || self.max_distance > u32::from(available_history)
        {
            return Err(HevcError::ReferenceMismatch);
        }
        Ok(())
    }
}
/// VUI `hrd_parameters(1, 0)`: buffering-model metadata a decoder does not
/// need. Parsed only to skip it exactly; every field stays range-checked.
fn hrd(b: &mut Bits<'_>) -> Result<(), HevcError> {
    let nal = b.flag()?;
    let vcl = b.flag()?;
    let mut sub_pic = false;
    if nal || vcl {
        sub_pic = b.flag()?;
        if sub_pic {
            b.read(8)?;
            b.read(5)?;
            b.read(1)?;
            b.read(5)?;
        }
        b.read(8)?; // bit_rate_scale, cpb_size_scale
        if sub_pic {
            b.read(4)?;
        }
        b.read(15)?; // three delay lengths
    }
    // One temporal sub-layer: `vui` refuses HRD when more are declared.
    let within_cvs = b.flag()? || b.flag()?;
    let low_delay = if within_cvs {
        b.ue(2047)?;
        false
    } else {
        b.flag()?
    };
    let cpb_count = if low_delay { 0 } else { b.ue(31)? };
    for present in [nal, vcl] {
        if present {
            for _ in 0..=cpb_count {
                b.ue(u32::MAX - 1)?;
                b.ue(u32::MAX - 1)?;
                if sub_pic {
                    b.ue(u32::MAX - 1)?;
                    b.ue(u32::MAX - 1)?;
                }
                b.flag()?;
            }
        }
    }
    Ok(())
}
fn timing(b: &mut Bits<'_>) -> Result<(), HevcError> {
    if b.read(32)? == 0 || b.read(32)? == 0 {
        return Err(HevcError::UnsupportedSyntax);
    }
    if b.flag()? {
        b.ue(u32::MAX - 1)?;
    }
    Ok(())
}
#[derive(Clone, Copy)]
pub(super) struct Vps {
    pub id: u32,
    pub profile: Profile,
    pub dpb: u8,
    pub sub_layers_minus1: u32,
}
pub(super) fn vps(nal: Nal<'_>, max_dpb: u8) -> Result<Vps, HevcError> {
    let mut b = nal.bits()?;
    let id = b.read(4)?;
    b.expect(2, 3)?; // Base layer internal and available.
    b.expect(6, 0)?;
    let sub_layers_minus1 = sub_layers(&mut b)?;
    b.expect(16, 0xffff)?;
    let profile = profile(&mut b, sub_layers_minus1)?;
    let dpb = ordering(&mut b, max_dpb, sub_layers_minus1)?;
    b.expect(6, 0)?;
    b.ue(0)?; // No additional layers or layer sets.
    if b.flag()? {
        timing(&mut b)?;
        b.ue(0)?;
    } // HRD is not in this low-delay subset.
    b.expect(1, 0)?;
    b.end()?;
    Ok(Vps {
        id,
        profile,
        dpb,
        sub_layers_minus1,
    })
}
#[derive(Clone, Copy)]
pub(super) struct Sps {
    pub id: u32,
    pub profile: Profile,
    pub dpb: u8,
    pub poc_bits: u8,
    pub temporal_mvp: bool,
    pub sao: bool,
    pub ctb_log2: u8,
    pub cb_depth: u32,
    pub rps_count: u32,
    pub rps: [Rps; MAX_SPS_RPS],
    /// The long-term syntax is present (with no SPS candidates): each
    /// slice then signals its long-term picture count, which must be zero.
    pub long_term: bool,
}
pub(super) fn sps(
    nal: Nal<'_>,
    vps: Vps,
    config: CodecConfiguration,
    limits: ProtocolLimits,
    max_dpb: u8,
) -> Result<Sps, HevcError> {
    let mut b = nal.bits()?;
    if b.read(4)? != vps.id {
        return Err(HevcError::ParameterSetReference);
    }
    let sub_layers_minus1 = sub_layers(&mut b)?;
    if sub_layers_minus1 > vps.sub_layers_minus1 {
        return Err(HevcError::ParameterSetReference);
    }
    let profile = profile(&mut b, sub_layers_minus1)?;
    if profile != vps.profile {
        return Err(HevcError::ParameterSetReference);
    }
    let id = b.ue(15)?;
    if b.ue(3)? != 1 {
        return Err(HevcError::UnsupportedSyntax);
    }
    let width = b.ue(limits.max_dimension_pixels())?;
    let height = b.ue(limits.max_dimension_pixels())?;
    limits
        .validate_coded_dimensions(width, height)
        .map_err(|_| HevcError::Limit)?;
    let g = config.geometry();
    if width != g.coded_width() || height != g.coded_height() {
        return Err(HevcError::GeometryMismatch);
    }
    let mut crop = [0; 4];
    if b.flag()? {
        for value in &mut crop {
            *value = b.ue(limits.max_dimension_pixels())?;
        }
    }
    // The current geometry contract has no origin field; only right/bottom padding is representable.
    if crop[0] != 0
        || crop[2] != 0
        || width.checked_sub(crop[1] * 2) != Some(g.crop_width())
        || height.checked_sub(crop[3] * 2) != Some(g.crop_height())
    {
        return Err(HevcError::GeometryMismatch);
    }
    b.ue(0)?;
    b.ue(0)?; // Eight-bit luma and chroma only.
    let poc_bits = u8::try_from(b.ue(12)? + 4).map_err(|_| HevcError::Limit)?;
    let dpb = ordering(&mut b, max_dpb, sub_layers_minus1)?;
    if dpb > vps.dpb {
        return Err(HevcError::DpbLimit);
    }
    let min_coding = b.ue(3)? + 3;
    let cb_depth = b.ue(3)?;
    let ctb = min_coding + cb_depth;
    if !(4..=6).contains(&ctb)
        || !width.is_multiple_of(1 << min_coding)
        || !height.is_multiple_of(1 << min_coding)
    {
        return Err(HevcError::UnsupportedSyntax);
    }
    let min_transform = b.ue(min_coding - 3)? + 2;
    let max_tb = min_transform + b.ue(ctb.min(5) - min_transform)?;
    if max_tb > ctb {
        return Err(HevcError::UnsupportedSyntax);
    }
    b.ue(ctb - min_transform)?;
    b.ue(ctb - min_transform)?;
    if b.flag()? {
        b.expect(1, 0)?;
    } // Default scaling lists only.
    b.flag()?; // AMP.
    let sao = b.flag()?;
    b.expect(1, 0)?; // No PCM.
    // Low-delay references are declared inline by each P slice (x265) or as
    // SPS sets the slice selects (NVENC); never with inter-RPS prediction.
    let rps_count = b.ue(MAX_SPS_RPS_COUNT)?;
    let mut rps = [Rps::default(); MAX_SPS_RPS];
    for (index, set) in rps.iter_mut().take(rps_count as usize).enumerate() {
        if index > 0 {
            b.expect(1, 0)?;
        }
        *set = Rps::parse(&mut b, dpb)?;
    }
    // No long-term references. Intel's Media Foundation HEVC transform
    // declares the syntax with no SPS candidates and never uses it; slices
    // must then say so (`slice` reads a zero long-term count).
    let long_term = b.flag()?;
    if long_term {
        b.ue(0)?;
    }
    let temporal_mvp = b.flag()?;
    b.flag()?; // Strong intra smoothing.
    if !b.flag()? {
        return Err(HevcError::ColorMismatch);
    }
    vui(&mut b, config, sub_layers_minus1)?;
    b.expect(1, 0)?; // No range, multiview, 3D, SCC or unknown extensions.
    b.end()?;
    Ok(Sps {
        id,
        profile,
        dpb,
        poc_bits,
        temporal_mvp,
        sao,
        ctb_log2: u8::try_from(ctb).map_err(|_| HevcError::Limit)?,
        cb_depth,
        rps_count,
        rps,
        long_term,
    })
}
fn vui(
    b: &mut Bits<'_>,
    config: CodecConfiguration,
    sub_layers_minus1: u32,
) -> Result<(), HevcError> {
    if b.flag()? {
        match b.read(8)? {
            0 | 1 => {}
            255 => {
                if b.read(16)? != 1 || b.read(16)? != 1 {
                    return Err(HevcError::GeometryMismatch);
                }
            }
            _ => return Err(HevcError::GeometryMismatch),
        }
    }
    if b.flag()? {
        b.flag()?;
    }
    if !b.flag()? {
        return Err(HevcError::ColorMismatch);
    }
    b.read(3)?;
    let full = b.flag()?;
    if !b.flag()? {
        return Err(HevcError::ColorMismatch);
    }
    let color = config.color();
    let primaries = match color.primaries {
        ColorPrimaries::Bt709 => 1,
        ColorPrimaries::Bt2020 => 9,
    };
    let transfer = match color.transfer {
        TransferFunction::Bt709 => 1,
        TransferFunction::Srgb => 13,
        _ => return Err(HevcError::ColorMismatch),
    };
    let matrix = match color.matrix {
        ColorMatrix::Bt709 => 1,
        ColorMatrix::Bt2020Ncl => 9,
    };
    if b.read(8)? != primaries
        || b.read(8)? != transfer
        || b.read(8)? != matrix
        || full != (color.range == ColorRange::Full)
    {
        return Err(HevcError::ColorMismatch);
    }
    if b.flag()? {
        b.ue(5)?;
        b.ue(5)?;
    }
    b.flag()?;
    b.expect(1, 0)?;
    b.flag()?; // Progressive frames only.
    if b.flag()? {
        for _ in 0..4 {
            b.ue(0)?;
        }
    } // No second, hidden display crop.
    if b.flag()? {
        timing(b)?;
        if b.flag()? {
            // HRD timing per declared sub-layer is not parsed; refuse it.
            if sub_layers_minus1 != 0 {
                return Err(HevcError::UnsupportedSyntax);
            }
            hrd(b)?;
        }
    }
    if b.flag()? {
        b.read(3)?;
        b.ue(4095)?;
        b.ue(16)?;
        b.ue(16)?;
        b.ue(16)?;
        b.ue(16)?;
    }
    Ok(())
}
#[derive(Clone, Copy)]
// These independent syntax flags mirror H.265, not application lifecycle state.
#[allow(clippy::struct_excessive_bools)]
pub(super) struct Pps {
    pub id: u32,
    pub output: bool,
    pub cabac_init: bool,
    pub chroma_offsets: bool,
    pub entropy_sync: bool,
    pub loop_filter: bool,
    pub deblock_override: bool,
    pub deblock_disabled: bool,
    pub init_qp: i32,
    /// `num_ref_idx_l{0,1}_default_active_minus1`, each at most 1.
    pub active_minus1: [u32; 2],
    /// Slices carry list-modification flags (which must be zero).
    pub lists_modification: bool,
}
pub(super) fn pps(nal: Nal<'_>, sps: Sps) -> Result<Pps, HevcError> {
    let mut b = nal.bits()?;
    let id = b.ue(63)?;
    if b.ue(15)? != sps.id {
        return Err(HevcError::ParameterSetReference);
    }
    b.expect(1, 0)?; // One independent slice per access unit.
    let output = b.flag()?;
    b.expect(3, 0)?;
    b.flag()?;
    let cabac_init = b.flag()?;
    // At most two active past references per list: x265, NVENC and
    // VideoToolbox use one, Intel's Media Foundation transform two.
    let active_minus1 = [b.ue(1)?, b.ue(1)?];
    let init_qp = b.se(-26, 25)? + 26;
    b.flag()?;
    b.flag()?;
    if b.flag()? {
        b.ue(sps.cb_depth)?;
    }
    b.se(-12, 12)?;
    b.se(-12, 12)?;
    let chroma_offsets = b.flag()?;
    b.expect(2, 0)?; // Weighted P/B prediction is outside the initial subset.
    b.flag()?;
    b.expect(1, 0)?; // No tiles.
    let entropy_sync = b.flag()?;
    let loop_filter = b.flag()?;
    let mut deblock_override = false;
    let mut deblock_disabled = false;
    if b.flag()? {
        deblock_override = b.flag()?;
        deblock_disabled = b.flag()?;
        if !deblock_disabled {
            b.se(-6, 6)?;
            b.se(-6, 6)?;
        }
    }
    b.expect(1, 0)?; // No custom scaling list.
    // List-modification syntax may be present; `slice` refuses any actual
    // modification.
    let lists_modification = b.flag()?;
    b.ue(u32::from(sps.ctb_log2) - 2)?;
    b.expect(1, 0)?;
    b.expect(1, 0)?; // No slice-header or PPS extensions.
    b.end()?;
    Ok(Pps {
        id,
        output,
        cabac_init,
        chroma_offsets,
        entropy_sync,
        loop_filter,
        deblock_override,
        deblock_disabled,
        init_qp,
        active_minus1,
        lists_modification,
    })
}
/// A P or low-delay B slice's list sizes and prediction controls: at most
/// two active references per list, no list modification, and a collocated
/// picture inside its active list.
fn reference_lists(
    b: &mut Bits<'_>,
    pps: Pps,
    bi: bool,
    used: u32,
    temporal_mvp: bool,
) -> Result<(), HevcError> {
    let mut active = pps.active_minus1;
    if b.flag()? {
        active[0] = b.ue(1)?;
        if bi {
            active[1] = b.ue(1)?;
        }
    }
    if pps.lists_modification && used > 1 {
        b.expect(1, 0)?; // No L0 modification.
        if bi {
            b.expect(1, 0)?; // No L1 modification.
        }
    }
    if bi {
        b.flag()?; // mvd_l1_zero_flag
    }
    if pps.cabac_init {
        b.flag()?;
    }
    if temporal_mvp {
        let from_l0 = !bi || b.flag()?;
        let list = active[usize::from(!from_l0)];
        if list > 0 {
            b.ue(list)?; // collocated_ref_idx
        }
    }
    b.ue(4)?; // five_minus_max_num_merge_cand
    Ok(())
}
/// Checks the complete independent slice header; CABAC data belongs to the decoder.
pub(super) fn slice(
    nal: Nal<'_>,
    sps: Sps,
    pps: Pps,
    config: CodecConfiguration,
    available_history: u8,
) -> Result<u32, HevcError> {
    let idr = matches!(nal.kind, 19 | 20);
    let mut b = nal.bits()?;
    b.expect(1, 1)?;
    if idr {
        b.flag()?;
    }
    if b.ue(63)? != pps.id {
        return Err(HevcError::ParameterSetReference);
    }
    // IDR pictures are I slices. Others are P, or low-delay B (Intel's
    // Media Foundation transform): every reference of both lists is a past
    // picture (`Rps::parse` refuses future ones), so nothing reorders.
    let kind = b.ue(2)?;
    if (idr && kind != 2) || (!idr && kind == 2) {
        return Err(HevcError::PictureMismatch);
    }
    let bi = kind == 0;
    if pps.output {
        b.expect(1, 1)?;
    }
    let mut poc = 0;
    let mut used = 0;
    let mut temporal_mvp = false;
    if !idr {
        poc = b.read(sps.poc_bits)?;
        let set = if b.flag()? {
            // short_term_ref_pic_set_sps_flag: select one admitted SPS set.
            let index = match sps.rps_count {
                0 => return Err(HevcError::ReferenceMismatch),
                1 => 0,
                n => b.read(
                    u8::try_from(32 - (n - 1).leading_zeros()).map_err(|_| HevcError::Limit)?,
                )?,
            };
            *sps.rps
                .get(usize::try_from(index).map_err(|_| HevcError::Limit)?)
                .filter(|_| index < sps.rps_count)
                .ok_or(HevcError::ReferenceMismatch)?
        } else {
            if sps.rps_count > 0 {
                b.expect(1, 0)?; // No inter-RPS prediction for the inline set.
            }
            Rps::parse(&mut b, sps.dpb)?
        };
        set.check(available_history)?;
        used = set.used;
        if sps.long_term {
            b.ue(0)?; // num_long_term_pics (no SPS candidates to select)
        }
        if sps.temporal_mvp {
            temporal_mvp = b.flag()?;
        }
    }
    let (mut sao_luma, mut sao_chroma) = (false, false);
    if sps.sao {
        sao_luma = b.flag()?;
        sao_chroma = b.flag()?;
    }
    if !idr {
        reference_lists(&mut b, pps, bi, used, temporal_mvp)?;
    }
    b.se(-pps.init_qp, 51 - pps.init_qp)?;
    if pps.chroma_offsets {
        b.se(-12, 12)?;
        b.se(-12, 12)?;
    }
    let mut deblock_disabled = pps.deblock_disabled;
    if pps.deblock_override && b.flag()? {
        deblock_disabled = b.flag()?;
        if !deblock_disabled {
            b.se(-6, 6)?;
            b.se(-6, 6)?;
        }
    }
    if pps.loop_filter && (sao_luma || sao_chroma || !deblock_disabled) {
        b.flag()?;
    }
    if pps.entropy_sync {
        let rows = config.geometry().coded_height().div_ceil(1 << sps.ctb_log2);
        let count = b.ue(rows - 1)?;
        if count != 0 {
            let bits = u8::try_from(b.ue(31)? + 1).map_err(|_| HevcError::Limit)?;
            let mut total = 0_u64;
            for _ in 0..count {
                total += u64::from(b.read(bits)?) + 1;
                if total >= nal.bytes.len() as u64 {
                    return Err(HevcError::Limit);
                }
            }
        }
    }
    b.alignment()?;
    // Actual decoded validity is not established by an admitted header.
    if !b.has_bytes() {
        return Err(HevcError::Truncated);
    }
    Ok(poc)
}
