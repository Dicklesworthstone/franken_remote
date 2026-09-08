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
fn profile(b: &mut Bits<'_>) -> Result<Profile, HevcError> {
    let mut bytes = [0; 12];
    for byte in &mut bytes {
        *byte = u8::try_from(b.read(8)?).map_err(|_| HevcError::Limit)?;
    }
    // Main profile, default profile space. No interlaced-source claim.
    if bytes[0] & 0xdf != 1 || bytes[5] & 0x40 != 0 || bytes[11] == 0 {
        return Err(HevcError::UnsupportedSyntax);
    }
    Ok(Profile(bytes))
}
fn ordering(b: &mut Bits<'_>, max_dpb: u8) -> Result<u8, HevcError> {
    b.flag()?; // One temporal layer: the presence flag cannot change the loop count.
    let dpb = b.ue(15)? + 1;
    if dpb > u32::from(max_dpb) || b.ue(15)? != 0 {
        return Err(HevcError::DpbLimit);
    }
    b.ue(u32::MAX - 1)?;
    u8::try_from(dpb).map_err(|_| HevcError::DpbLimit)
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
}
pub(super) fn vps(nal: Nal<'_>, max_dpb: u8) -> Result<Vps, HevcError> {
    let mut b = nal.bits()?;
    let id = b.read(4)?;
    b.expect(2, 3)?; // Base layer internal and available.
    b.expect(6, 0)?;
    b.expect(3, 0)?;
    b.expect(1, 1)?;
    b.expect(16, 0xffff)?;
    let profile = profile(&mut b)?;
    let dpb = ordering(&mut b, max_dpb)?;
    b.expect(6, 0)?;
    b.ue(0)?; // No additional layers or layer sets.
    if b.flag()? {
        timing(&mut b)?;
        b.ue(0)?;
    } // HRD is not in this low-delay subset.
    b.expect(1, 0)?;
    b.end()?;
    Ok(Vps { id, profile, dpb })
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
    b.expect(3, 0)?;
    b.expect(1, 1)?;
    let profile = profile(&mut b)?;
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
    let dpb = ordering(&mut b, max_dpb)?;
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
    b.ue(0)?; // References are declared inline by each P slice, never predicted RPS tables.
    b.expect(1, 0)?; // No long-term references.
    let temporal_mvp = b.flag()?;
    b.flag()?; // Strong intra smoothing.
    if !b.flag()? {
        return Err(HevcError::ColorMismatch);
    }
    vui(&mut b, config)?;
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
    })
}
fn vui(b: &mut Bits<'_>, config: CodecConfiguration) -> Result<(), HevcError> {
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
        b.expect(1, 0)?;
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
    b.ue(0)?;
    b.ue(0)?; // At most one active previous reference.
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
    b.expect(1, 0)?;
    b.expect(1, 0)?; // No custom scaling list or reference list modification.
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
    })
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
    if b.ue(2)? != if idr { 2 } else { 1 } {
        return Err(HevcError::PictureMismatch);
    }
    if pps.output {
        b.expect(1, 1)?;
    }
    let mut poc = 0;
    if !idr {
        poc = b.read(sps.poc_bits)?;
        b.expect(1, 0)?; // No SPS reference-picture set table.
        let past = b.ue(u32::from(sps.dpb) - 1)?;
        if past == 0 || b.ue(15)? != 0 {
            return Err(HevcError::ReferenceMismatch);
        }
        let mut distance = 0;
        for index in 0..past {
            distance += b.ue(32767)? + 1;
            let used = b.flag()?;
            if distance > u32::from(available_history) || (index == 0 && (distance != 1 || !used)) {
                return Err(HevcError::ReferenceMismatch);
            }
        }
        // Additional retained RPS entries are legal, but only L0[0] can be
        // active: PPS/slice active-count and list-modification checks enforce it.
        if sps.temporal_mvp {
            b.flag()?;
        }
    }
    let (mut sao_luma, mut sao_chroma) = (false, false);
    if sps.sao {
        sao_luma = b.flag()?;
        sao_chroma = b.flag()?;
    }
    if !idr {
        if b.flag()? {
            b.ue(0)?;
        }
        if pps.cabac_init {
            b.flag()?;
        }
        // With one L0 reference, collocated_ref_idx is inferred zero.
        b.ue(4)?;
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
