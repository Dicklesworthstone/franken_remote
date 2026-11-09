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
        let nal = self.next_framed()?;
        if let Some(nal) = nal {
            // Inspect the complete escaped payload, including CABAC bytes.
            // Conversion must not expose a hidden start code to the decoder.
            Bits::new(&nal.bytes[2..]).validate_escaping()?;
        }
        Ok(nal)
    }
    // Framing only, used exclusively while normalizing a HOST encoder result.
    // The public guard checks the entire retained payload after normalization;
    // viewer input and ordinary framing conversion always use `next` above.
    fn next_framed(&mut self) -> Result<Option<Nal<'a>>, HevcError> {
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

// Preserve the host's previous Annex-B normalization without manufacturing
// delimiters and scanning the whole compressed picture to find them again.
// Only the guard's normalization/admission method exposes this output.
pub(super) fn encoder_sample(bytes: &[u8], limits: ProtocolLimits) -> Result<Vec<u8>, HevcError> {
    limits
        .validate_access_unit_len(bytes.len())
        .map_err(|_| HevcError::Limit)?;
    let mut units = LengthPrefixed::new(bytes);
    let mut retained = [&[][..]; MAX_NALS];
    let mut count = 0;
    let mut size = 0_usize;
    while let Some(nal) = units.next_framed()? {
        // AnnexB::next treated trailing zero bytes as delimiter/padding, not
        // picture data. Retain that normalization, including its truncation
        // check, so existing qualified native outputs keep identical bytes.
        let mut data = nal.bytes;
        while data.last() == Some(&0) {
            data = &data[..data.len() - 1];
        }
        let nal = Nal::new(data)?;
        if matches!(nal.kind, 39 | 40) {
            continue;
        }
        size = size
            .checked_add(nal.bytes.len())
            .and_then(|n| n.checked_add(4))
            .ok_or(HevcError::Limit)?;
        limits
            .validate_access_unit_len(size)
            .map_err(|_| HevcError::Limit)?;
        retained[count] = nal.bytes;
        count += 1;
    }
    pack(&retained[..count], size, false)
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
    pack(&retained[..count], size, annex_b)
}

fn pack(retained: &[&[u8]], size: usize, annex_b: bool) -> Result<Vec<u8>, HevcError> {
    if size == 0 {
        return Err(HevcError::Framing);
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(size)
        .map_err(|_| HevcError::Allocation)?;
    for nal in retained {
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
