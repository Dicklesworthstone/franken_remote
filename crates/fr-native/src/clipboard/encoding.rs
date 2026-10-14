//! Exact native text conversion only. The network representation remains UTF-8.
//! X11 STRING is ISO Latin-1 graphics plus TAB/NEWLINE (ICCCM 2.6.2).
#![forbid(unsafe_code)]
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

/// Decide representability once during native preparation. An unsupported
/// character is never replaced with '?' or advertised as an available target.
pub(super) fn latin1_len(text: &str) -> Option<usize> {
    text.chars().try_fold(0_usize, |size, ch| {
        let byte = u8::try_from(u32::from(ch)).ok()?;
        latin1_text(byte).then_some(())?;
        size.checked_add(1)
    })
}

/// One native property, not a second copy of the selected item. The offset is
/// in the ORIGINAL UTF-8 allocation; concurrent readers retain their own exact
/// encoding and immutable selection. Conversion never scans the whole suffix.
pub(super) struct Latin1Chunk {
    bytes: [u8; super::CHUNK],
    len: usize,
    pub(super) next: usize,
}
impl Latin1Chunk {
    pub(super) fn at(source: &[u8], offset: usize) -> Option<Self> {
        if offset > source.len() {
            return None;
        }
        let mut chunk = Self {
            bytes: [0; super::CHUNK],
            len: 0,
            next: offset,
        };
        while chunk.next < source.len() && chunk.len < super::CHUNK {
            let first = source[chunk.next];
            let (byte, consumed) = match first {
                0..=127 => (first, 1),
                0xc2 | 0xc3 => {
                    let tail = *source.get(chunk.next + 1)?;
                    if !(0x80..=0xbf).contains(&tail) {
                        return None;
                    }
                    (((first & 3) << 6) | (tail & 63), 2)
                }
                _ => return None,
            };
            if !latin1_text(byte) {
                return None;
            }
            chunk.bytes[chunk.len] = byte;
            chunk.len += 1;
            chunk.next += consumed;
        }
        Some(chunk)
    }
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}
impl Drop for Latin1Chunk {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}
