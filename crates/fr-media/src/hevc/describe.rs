//! Host-only SPS normalization: write the colour description a HOST encoder
//! left out. Intel's Media Foundation HEVC transform signals video range but
//! no primaries, transfer or matrix, and refuses every setting that would add
//! them. The host knows what it fed the encoder (its own converter's output),
//! so it states exactly that in the VUI.
//!
//! This walker trusts nothing it produces: the result is admitted by the same
//! strict `HevcGuard` as any encoder output, and a walk it cannot follow is an
//! error, never a guess. It is not a viewer-input API.
use super::HevcError;
use super::nal::MAX_PARAMETER_BYTES;
use crate::config::{ColorInfo, ColorMatrix, ColorPrimaries, ColorRange, TransferFunction};

/// Emulation-prevention-free payload bits after the two-byte NAL header.
struct Reader {
    rbsp: Vec<u8>,
    at: usize,
}
impl Reader {
    fn bit(&mut self) -> Result<u32, HevcError> {
        let byte = self.rbsp.get(self.at / 8).ok_or(HevcError::Truncated)?;
        let bit = (byte >> (7 - self.at % 8)) & 1;
        self.at += 1;
        Ok(u32::from(bit))
    }
    fn read(&mut self, count: u32) -> Result<u32, HevcError> {
        let mut value = 0;
        for _ in 0..count {
            value = (value << 1) | self.bit()?;
        }
        Ok(value)
    }
    fn skip(&mut self, count: usize) -> Result<(), HevcError> {
        if self.at + count > self.rbsp.len() * 8 {
            return Err(HevcError::Truncated);
        }
        self.at += count;
        Ok(())
    }
    fn ue(&mut self) -> Result<u32, HevcError> {
        let mut leading = 0;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return Err(HevcError::Limit);
            }
        }
        Ok(((1_u32 << leading) - 1) + self.read(leading)?)
    }
}

#[derive(Default)]
struct Writer {
    bytes: Vec<u8>,
    bits: usize,
}
impl Writer {
    fn put(&mut self, bit: u32) {
        if self.bits.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if bit != 0 {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (7 - self.bits % 8);
        }
        self.bits += 1;
    }
    fn write(&mut self, value: u32, count: u32) {
        for shift in (0..count).rev() {
            self.put((value >> shift) & 1);
        }
    }
    fn copy(&mut self, from: &Reader, range: core::ops::Range<usize>) {
        for at in range {
            self.put(u32::from((from.rbsp[at / 8] >> (7 - at % 8)) & 1));
        }
    }
}

fn unescape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut zeroes = 0;
    for &byte in bytes {
        if zeroes >= 2 && byte == 3 {
            zeroes = 0;
            continue;
        }
        zeroes = if byte == 0 { zeroes + 1 } else { 0 };
        out.push(byte);
    }
    out
}

fn escape(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeroes = 0;
    for &byte in rbsp {
        if zeroes >= 2 && byte <= 3 {
            out.push(3);
            zeroes = 0;
        }
        zeroes = if byte == 0 { zeroes + 1 } else { 0 };
        out.push(byte);
    }
}

/// `profile_tier_level(1, max_sub_layers_minus1)`.
fn profile_tier_level(r: &mut Reader, sub_layers_minus1: u32) -> Result<(), HevcError> {
    r.skip(88 + 8)?;
    let mut present = [(false, false); 7];
    for flags in present.iter_mut().take(sub_layers_minus1 as usize) {
        *flags = (r.bit()? != 0, r.bit()? != 0);
    }
    if sub_layers_minus1 > 0 {
        r.skip(2 * (8 - sub_layers_minus1 as usize))?;
    }
    for (profile, level) in present.iter().take(sub_layers_minus1 as usize) {
        if *profile {
            r.skip(88)?;
        }
        if *level {
            r.skip(8)?;
        }
    }
    Ok(())
}

/// Walks an SPS to the VUI's `video_signal_type_present_flag`. Syntax this
/// walker does not follow (scaling lists, PCM, inter-RPS prediction) is an
/// error; the guard refuses those anyway.
fn walk_to_signal_type(r: &mut Reader) -> Result<bool, HevcError> {
    r.skip(4)?;
    let sub_layers_minus1 = r.read(3)?;
    if sub_layers_minus1 > 6 {
        return Err(HevcError::UnsupportedSyntax);
    }
    r.skip(1)?;
    profile_tier_level(r, sub_layers_minus1)?;
    r.ue()?;
    if r.ue()? == 3 {
        r.skip(1)?;
    }
    r.ue()?;
    r.ue()?;
    if r.bit()? != 0 {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    r.ue()?;
    r.ue()?;
    let poc_bits = r.ue()? + 4;
    if poc_bits > 16 {
        return Err(HevcError::Limit);
    }
    let all_layers = r.bit()? != 0;
    let layers = if all_layers { sub_layers_minus1 + 1 } else { 1 };
    for _ in 0..layers * 3 {
        r.ue()?;
    }
    for _ in 0..6 {
        r.ue()?;
    }
    if r.bit()? != 0 && r.bit()? != 0 {
        return Err(HevcError::UnsupportedSyntax); // Explicit scaling lists.
    }
    r.skip(2)?; // AMP, SAO.
    if r.bit()? != 0 {
        return Err(HevcError::UnsupportedSyntax); // PCM.
    }
    let sets = r.ue()?;
    if sets > 64 {
        return Err(HevcError::Limit);
    }
    for index in 0..sets {
        if index > 0 && r.bit()? != 0 {
            return Err(HevcError::UnsupportedSyntax); // Inter-RPS prediction.
        }
        let (past, future) = (r.ue()?, r.ue()?);
        if past + future > 16 {
            return Err(HevcError::Limit);
        }
        for _ in 0..past + future {
            r.ue()?;
            r.skip(1)?;
        }
    }
    if r.bit()? != 0 {
        let long_term = r.ue()?;
        if long_term > 32 {
            return Err(HevcError::Limit);
        }
        for _ in 0..long_term {
            r.skip(poc_bits as usize + 1)?;
        }
    }
    r.skip(2)?; // Temporal MVP, strong intra smoothing.
    if r.bit()? == 0 {
        return Ok(false); // No VUI.
    }
    if r.bit()? != 0 && r.read(8)? == 255 {
        r.skip(32)?;
    }
    if r.bit()? != 0 {
        r.skip(1)?;
    }
    Ok(true)
}

fn codes(color: ColorInfo) -> Result<[u32; 3], HevcError> {
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
    Ok([primaries, transfer, matrix])
}

/// The SPS NAL (two-byte header, escaped payload) with `color` written as
/// its VUI colour description when the encoder wrote none. An SPS that
/// already describes its colour is returned unchanged (admission compares
/// it); one without a VUI, or whose signalled range differs from `color`,
/// is refused rather than rewritten.
pub fn describe_encoder_colour(sps: &[u8], color: ColorInfo) -> Result<Vec<u8>, HevcError> {
    if sps.len() < 3 || sps.len() > MAX_PARAMETER_BYTES || (sps[0] >> 1) & 63 != 33 {
        return Err(HevcError::UnsupportedSyntax);
    }
    let full = u32::from(color.range == ColorRange::Full);
    let [primaries, transfer, matrix] = codes(color)?;
    let mut r = Reader {
        rbsp: unescape(&sps[2..]),
        at: 0,
    };
    if !walk_to_signal_type(&mut r)? {
        return Err(HevcError::ColorMismatch);
    }
    let site = r.at;
    let mut w = Writer::default();
    let r_bits = r.rbsp.len() * 8;
    // The rbsp_stop_one_bit: the last set bit of the payload.
    let stop = (0..r_bits)
        .rev()
        .find(|&at| (r.rbsp[at / 8] >> (7 - at % 8)) & 1 == 1)
        .ok_or(HevcError::Truncated)?;
    if r.bit()? == 1 {
        r.skip(3)?;
        if r.bit()? != full {
            return Err(HevcError::ColorMismatch);
        }
        if r.bit()? == 1 {
            return Ok(sps.to_vec());
        }
        // Everything through the range flag, then the description.
        w.copy(&r, 0..r.at - 1);
    } else {
        w.copy(&r, 0..site);
        w.put(1); // video_signal_type_present_flag
        w.write(5, 3); // video_format: unspecified
        w.put(full);
    }
    w.put(1); // colour_description_present_flag
    w.write(primaries, 8);
    w.write(transfer, 8);
    w.write(matrix, 8);
    if r.at > stop {
        return Err(HevcError::Truncated);
    }
    w.copy(&r, r.at..stop);
    w.put(1); // rbsp_stop_one_bit
    while !w.bits.is_multiple_of(8) {
        w.put(0);
    }
    let mut out = Vec::with_capacity(w.bytes.len() + w.bytes.len() / 2 + 2);
    out.extend_from_slice(&sps[..2]);
    escape(&w.bytes, &mut out);
    if out.len() > MAX_PARAMETER_BYTES {
        return Err(HevcError::Limit);
    }
    Ok(out)
}

/// `describe_encoder_colour` applied to every SPS of one Annex B access unit;
/// every other NAL is copied unchanged with a four-byte start code.
pub fn describe_encoder_colour_annex_b(
    bytes: &[u8],
    color: ColorInfo,
) -> Result<Vec<u8>, HevcError> {
    let mut units = super::nal::AnnexB::new(bytes)?;
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len() + 64)
        .map_err(|_| HevcError::Allocation)?;
    while let Some(nal) = units.next()? {
        out.extend_from_slice(&[0, 0, 0, 1]);
        if nal.kind == 33 {
            out.extend_from_slice(&describe_encoder_colour(nal.bytes, color)?);
        } else {
            out.extend_from_slice(nal.bytes);
        }
    }
    Ok(out)
}
