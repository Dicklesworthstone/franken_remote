//! Explicit four-byte-length-prefixed wire framing; never guess from payload bytes.
use super::{
    HevcError,
    nal::{AnnexB, Bits, MAX_NALS, Nal},
};
use fr_core::limits::ProtocolLimits;

#[derive(Clone)]
pub(super) struct LengthPrefixed<'a> {
    bytes: &'a [u8],
    count: usize,
}
impl<'a> LengthPrefixed<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, count: 0 }
    }
    pub fn next(&mut self) -> Result<Option<Nal<'a>>, HevcError> {
        if self.bytes.is_empty() {
            return Ok(None);
        }
        if self.count == MAX_NALS {
            return Err(HevcError::Limit);
        }
        let prefix: [u8; 4] = self
            .bytes
            .get(..4)
            .ok_or(HevcError::Truncated)?
            .try_into()
            .map_err(|_| HevcError::Framing)?;
        let length = usize::try_from(u32::from_be_bytes(prefix)).map_err(|_| HevcError::Limit)?;
        let tail = &self.bytes[4..];
        let data = tail.get(..length).ok_or(HevcError::Truncated)?;
        let nal = Nal::new(data)?;
        // Inspect the complete escaped payload, including CABAC bytes. Otherwise
        // conversion to Annex B could expose a hidden start code to the decoder.
        Bits::new(&data[2..]).validate_escaping()?;
        self.bytes = &tail[length..];
        self.count += 1;
        Ok(Some(nal))
    }
}
#[derive(Clone)]
pub(super) enum Units<'a> {
    AnnexB(AnnexB<'a>),
    LengthPrefixed(LengthPrefixed<'a>),
}
impl<'a> Units<'a> {
    pub fn next(&mut self) -> Result<Option<Nal<'a>>, HevcError> {
        match self {
            Self::AnnexB(n) => n.next(),
            Self::LengthPrefixed(n) => n.next(),
        }
    }
}

/// Repackages backend Annex B as complete NALs with four-byte big-endian lengths.
/// This is a framing conversion, NOT HEVC admission; use `HevcGuard` as well.
/// Both input and expanded output are bounded before allocating the output.
pub fn annex_b_to_length_prefixed(
    bytes: &[u8],
    limits: ProtocolLimits,
) -> Result<Vec<u8>, HevcError> {
    limits
        .validate_access_unit_len(bytes.len())
        .map_err(|_| HevcError::Limit)?;
    repack(Units::AnnexB(AnnexB::new(bytes)?), limits, false, false)
}
/// Repackages canonical wire NALs for a native API requiring Annex B. Rejects
/// hidden start codes and malformed escaping, including inside CABAC data.
pub fn length_prefixed_to_annex_b(
    bytes: &[u8],
    limits: ProtocolLimits,
) -> Result<Vec<u8>, HevcError> {
    limits
        .validate_access_unit_len(bytes.len())
        .map_err(|_| HevcError::Limit)?;
    repack(
        Units::LengthPrefixed(LengthPrefixed::new(bytes)),
        limits,
        true,
        false,
    )
}
pub(super) fn hvc1_sample(bytes: &[u8], limits: ProtocolLimits) -> Result<Vec<u8>, HevcError> {
    repack(
        Units::LengthPrefixed(LengthPrefixed::new(bytes)),
        limits,
        false,
        true,
    )
}
fn repack(
    mut units: Units<'_>,
    limits: ProtocolLimits,
    annex_b: bool,
    strip_sets: bool,
) -> Result<Vec<u8>, HevcError> {
    let mut size = 0_usize;
    let mut retained = [&[][..]; MAX_NALS];
    let mut count = 0;
    while let Some(nal) = units.next()? {
        if strip_sets && (32..=34).contains(&nal.kind) {
            continue;
        }
        retained[count] = nal.bytes;
        count += 1;
        size = size
            .checked_add(nal.bytes.len())
            .and_then(|n| n.checked_add(4))
            .ok_or(HevcError::Limit)?;
        limits
            .validate_access_unit_len(size)
            .map_err(|_| HevcError::Limit)?;
    }
    if size == 0 {
        return Err(HevcError::Framing);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(size)
        .map_err(|_| HevcError::Allocation)?;
    for nal in &retained[..count] {
        let prefix = if annex_b {
            [0, 0, 0, 1]
        } else {
            u32::try_from(nal.len())
                .map_err(|_| HevcError::Limit)?
                .to_be_bytes()
        };
        output.extend_from_slice(&prefix);
        output.extend_from_slice(nal);
    }
    Ok(output)
}
