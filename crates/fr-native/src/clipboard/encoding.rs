//! Exact native text conversion only. The network representation remains UTF-8.
//! X11 STRING is ISO Latin-1 graphics plus TAB/NEWLINE (ICCCM 2.6.2).
use super::ReadError;

// Core X11 predefined atom, not an interned/user-selected identifier.
pub(super) const STRING: u32 = 31;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Encoding {
    Utf8,
    Latin1,
}
impl Encoding {
    pub(super) fn atom(self, utf8: u32) -> u32 {
        match self {
            Self::Utf8 => utf8,
            Self::Latin1 => STRING,
        }
    }
    /// Validate the entire native chunk and charge its expanded size BEFORE
    /// reserve/copy. No replacement characters, locale lookup or second buffer.
    pub(super) fn append(
        self,
        text: &mut Vec<u8>,
        bytes: &[u8],
        limit: usize,
    ) -> Result<(), ReadError> {
        let extra = match self {
            Self::Utf8 => bytes.len(),
            Self::Latin1 => bytes.iter().try_fold(0_usize, |size, &byte| {
                if !latin1_text(byte) {
                    return Err(ReadError::Unsupported);
                }
                size.checked_add(if byte < 128 { 1 } else { 2 })
                    .ok_or(ReadError::Limit)
            })?,
        };
        if text
            .len()
            .checked_add(extra)
            .is_none_or(|size| size > limit)
        {
            return Err(ReadError::Limit);
        }
        text.try_reserve_exact(extra)
            .map_err(|_| ReadError::Allocation)?;
        match self {
            Self::Utf8 => text.extend_from_slice(bytes),
            Self::Latin1 => {
                for &byte in bytes {
                    if byte < 128 {
                        text.push(byte);
                    } else {
                        text.extend_from_slice(&[0xc0 | (byte >> 6), 0x80 | (byte & 63)]);
                    }
                }
            }
        }
        Ok(())
    }
}
fn latin1_text(byte: u8) -> bool {
    matches!(byte, b'\t' | b'\n' | 32..=126 | 160..=255)
}
