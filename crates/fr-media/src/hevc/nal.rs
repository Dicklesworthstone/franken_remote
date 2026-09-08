//! Allocation-free HEVC byte-stream framing and bounded RBSP reads.
use super::HevcError;

pub(super) const MAX_NALS: usize = 256;
pub(super) const MAX_PARAMETER_BYTES: usize = 4096;

/// One NAL without its start code or length prefix. Payload is never logged.
#[derive(Clone, Copy)]
pub(super) struct Nal<'a> {
    pub bytes: &'a [u8],
    pub kind: u8,
}
impl<'a> Nal<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, HevcError> {
        if bytes.len() < 3 {
            return Err(HevcError::Truncated);
        }
        if bytes[0] & 0x81 != 0 || bytes[1] != 1 {
            return Err(HevcError::UnsupportedLayer);
        }
        Ok(Self {
            bytes,
            kind: (bytes[0] >> 1) & 63,
        })
    }
    pub fn bits(self) -> Result<Bits<'a>, HevcError> {
        if (32..=34).contains(&self.kind) && self.bytes.len() > MAX_PARAMETER_BYTES {
            return Err(HevcError::Limit);
        }
        Ok(Bits::new(&self.bytes[2..]))
    }
}

/// Mixed three/four-byte start codes, with Annex B leading/trailing zero bytes.
/// A zero run belongs to the delimiter, never to the preceding parameter set.
#[derive(Clone)]
pub(super) struct AnnexB<'a> {
    bytes: &'a [u8],
    cursor: usize,
    count: usize,
}
fn start_code(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut zeroes = 0;
    for (offset, byte) in bytes.get(from..)?.iter().enumerate() {
        if *byte == 1 && zeroes >= 2 {
            return Some((from + offset - zeroes, from + offset + 1));
        }
        zeroes = if *byte == 0 { zeroes + 1 } else { 0 };
    }
    None
}
impl<'a> AnnexB<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, HevcError> {
        let (begin, cursor) = start_code(bytes, 0).ok_or(HevcError::Framing)?;
        if bytes[..begin].iter().any(|b| *b != 0) {
            return Err(HevcError::Framing);
        }
        Ok(Self {
            bytes,
            cursor,
            count: 0,
        })
    }
    pub fn next(&mut self) -> Result<Option<Nal<'a>>, HevcError> {
        if self.cursor == self.bytes.len() {
            return Ok(None);
        }
        if self.count == MAX_NALS {
            return Err(HevcError::Limit);
        }
        let (end, next) =
            start_code(self.bytes, self.cursor).unwrap_or((self.bytes.len(), self.bytes.len()));
        let mut data = &self.bytes[self.cursor..end];
        self.cursor = next;
        self.count += 1;
        while data.last() == Some(&0) {
            data = &data[..data.len() - 1];
        }
        // A delimiter without a NAL (including a terminal delimiter) is invalid.
        if next == self.bytes.len() && end != next {
            return Err(HevcError::Truncated);
        }
        Nal::new(data).map(Some)
    }
}

/// Reads emulation-prevention bytes lazily. No allocation or attacker-sized loop.
pub(super) struct Bits<'a> {
    bytes: &'a [u8],
    cursor: usize,
    zeroes: u8,
    byte: u8,
    left: u8,
}
impl<'a> Bits<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            cursor: 0,
            zeroes: 0,
            byte: 0,
            left: 0,
        }
    }
    pub fn validate_escaping(mut self) -> Result<(), HevcError> {
        while self.cursor < self.bytes.len() {
            self.byte()?;
        }
        Ok(())
    }
    fn byte(&mut self) -> Result<u8, HevcError> {
        let mut byte = *self.bytes.get(self.cursor).ok_or(HevcError::Truncated)?;
        self.cursor += 1;
        if self.zeroes == 2 {
            if byte == 3 {
                byte = *self.bytes.get(self.cursor).ok_or(HevcError::Truncated)?;
                self.cursor += 1;
                if byte > 3 {
                    return Err(HevcError::EmulationPrevention);
                }
                self.zeroes = 0;
            } else if byte < 3 {
                return Err(HevcError::EmulationPrevention);
            }
        }
        self.zeroes = if byte == 0 { self.zeroes + 1 } else { 0 };
        Ok(byte)
    }
    pub fn read(&mut self, count: u8) -> Result<u32, HevcError> {
        if count > 32 {
            return Err(HevcError::Limit);
        }
        let mut value = 0;
        for _ in 0..count {
            if self.left == 0 {
                self.byte = self.byte()?;
                self.left = 8;
            }
            self.left -= 1;
            value = (value << 1) | u32::from((self.byte >> self.left) & 1);
        }
        Ok(value)
    }
    pub fn flag(&mut self) -> Result<bool, HevcError> {
        Ok(self.read(1)? != 0)
    }
    pub fn expect(&mut self, count: u8, expected: u32) -> Result<(), HevcError> {
        if self.read(count)? == expected {
            Ok(())
        } else {
            Err(HevcError::UnsupportedSyntax)
        }
    }
    pub fn ue(&mut self, max: u32) -> Result<u32, HevcError> {
        let mut leading = 0_u8;
        while !self.flag()? {
            leading += 1;
            if leading > 31 {
                return Err(HevcError::Limit);
            }
        }
        let value = ((1_u32 << leading) - 1)
            .checked_add(self.read(leading)?)
            .ok_or(HevcError::Limit)?;
        if value > max {
            Err(HevcError::Limit)
        } else {
            Ok(value)
        }
    }
    pub fn se(&mut self, min: i32, max: i32) -> Result<i32, HevcError> {
        let code = i64::from(self.ue(u32::MAX - 1)?);
        let value = if code & 1 == 0 {
            -(code / 2)
        } else {
            (code + 1) / 2
        };
        if value < i64::from(min) || value > i64::from(max) {
            return Err(HevcError::Limit);
        }
        i32::try_from(value).map_err(|_| HevcError::Limit)
    }
    pub fn alignment(&mut self) -> Result<(), HevcError> {
        self.expect(1, 1)?;
        while self.left != 0 {
            self.expect(1, 0)?;
        }
        Ok(())
    }
    pub fn at_trailing_bits(&self) -> bool {
        self.left == 0 && self.bytes.get(self.cursor..) == Some(&[0x80][..])
    }
    pub fn has_bytes(&self) -> bool {
        self.cursor < self.bytes.len()
    }
    pub fn end(mut self) -> Result<(), HevcError> {
        self.expect(1, 1)?;
        while self.left != 0 {
            self.expect(1, 0)?;
        }
        if self.cursor != self.bytes.len() {
            return Err(HevcError::UnsupportedSyntax);
        }
        Ok(())
    }
}
