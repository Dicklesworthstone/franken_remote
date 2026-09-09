//! Incremental framing for initial control and attached reliable FRD0 streams.
//!
//! QUIC reads are byte chunks, not records. This owner retains at most one
//! bounded record across arbitrary read boundaries. It never scans for a new
//! magic after failure and never allocates an unvalidated advertised length.
//! A complete record still needs its message codec, direction, and live session
//! checks. Framing alone does not implement the negotiation state machine.
use crate::{HEADER_BYTES, WireError};
use core::fmt;
use fr_core::limits::ProtocolLimits;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamError {
    Wire(WireError),
    InvalidPolicy,
    Allocation,
    Expired,
    ClockRegression,
    Closed,
}
impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl core::error::Error for StreamError {}

/// One record, including its header, is charged against `maximum`. Time values
/// are microseconds in the RECEIVER'S clock domain, not host capture times.
/// The deadline starts at the first byte and is never extended by trickle reads.
pub struct RecordStream {
    maximum: usize,
    lifetime: u64,
    binding: u32,
    header: [u8; HEADER_BYTES],
    header_len: usize,
    record: Vec<u8>,
    total: Option<usize>,
    until: Option<u64>,
    last_now: Option<u64>,
    failure: Option<StreamError>,
    closed: bool,
}
impl fmt::Debug for RecordStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecordStream")
            .field("maximum", &self.maximum)
            .field("buffered_bytes", &self.buffered_bytes())
            .field("allocated_bytes", &self.allocated_bytes())
            .field("closed", &self.closed)
            .field("failure", &self.failure)
            .finish_non_exhaustive()
    }
}
impl RecordStream {
    pub fn new(maximum: usize, binding: u32, lifetime_micros: u64) -> Result<Self, StreamError> {
        if binding == 0 {
            return Err(StreamError::InvalidPolicy);
        }
        Self::new_inner(maximum, binding, lifetime_micros)
    }
    /// Initial control only. Zero binding never admits media or input records.
    /// The complete record is capped at the smaller of `maximum` and the
    /// negotiation codec's limit, including before payload allocation.
    pub fn negotiation(maximum: usize, lifetime_micros: u64) -> Result<Self, StreamError> {
        Self::new_inner(
            maximum.min(crate::negotiation::MAX_RECORD),
            0,
            lifetime_micros,
        )
    }
    fn new_inner(maximum: usize, binding: u32, lifetime_micros: u64) -> Result<Self, StreamError> {
        if !(HEADER_BYTES..=ProtocolLimits::ABSOLUTE.max_control_message_bytes() as usize)
            .contains(&maximum)
            || !(1..=5_000_000).contains(&lifetime_micros)
        {
            return Err(StreamError::InvalidPolicy);
        }
        Ok(Self {
            maximum,
            lifetime: lifetime_micros,
            binding,
            header: [0; HEADER_BYTES],
            header_len: 0,
            record: Vec::new(),
            total: None,
            until: None,
            last_now: None,
            failure: None,
            closed: false,
        })
    }
    /// Consume at most one record. A zero return with a ready record means
    /// backpressure: process and `consume` it before reading more stream bytes.
    /// Keep unread bytes in the caller's already bounded transport read buffer.
    pub fn push(&mut self, bytes: &[u8], now: u64) -> Result<usize, StreamError> {
        self.tick(now)?;
        if bytes.is_empty() || self.ready() {
            return Ok(0);
        }
        if self.until.is_none() {
            let Some(until) = now.checked_add(self.lifetime) else {
                return self.fail(StreamError::Wire(WireError::ArithmeticOverflow));
            };
            self.until = Some(until);
        }
        let mut consumed = 0;
        if self.header_len < HEADER_BYTES {
            let n = (HEADER_BYTES - self.header_len).min(bytes.len());
            self.header[self.header_len..self.header_len + n].copy_from_slice(&bytes[..n]);
            self.header_len += n;
            consumed += n;
            if self.header_len < HEADER_BYTES {
                return Ok(consumed);
            }
            let total = match self.validate_header() {
                Ok(total) => total,
                Err(error) => return self.fail(StreamError::Wire(error)),
            };
            // Reuse the one bounded allocation. No allocation precedes header
            // validation. Charge capacity, not just the record's current length.
            if self.record.try_reserve_exact(total).is_err() {
                return self.fail(StreamError::Allocation);
            }
            if self.record.capacity() > self.maximum {
                return self.fail(StreamError::Allocation);
            }
            self.record.extend_from_slice(&self.header);
            self.total = Some(total);
        }
        let remaining = self.total.expect("validated header") - self.record.len();
        let n = remaining.min(bytes.len() - consumed);
        self.record
            .extend_from_slice(&bytes[consumed..consumed + n]);
        Ok(consumed + n)
    }
    fn validate_header(&self) -> Result<usize, WireError> {
        let h = &self.header;
        if &h[..4] != b"FRD0" {
            return Err(WireError::BadMagic);
        }
        if h[4..6] != [0, 0] {
            return Err(WireError::UnsupportedVersion);
        }
        if h[8..12] != [0; 4] {
            return Err(WireError::InvalidFlags);
        }
        if self.binding == 0
            && !matches!(
                u16::from_be_bytes([h[6], h[7]]),
                0x0001..=0x0003 | 0x0010 | 0x0011
            )
        {
            return Err(WireError::UnsupportedKind);
        }
        let word = |i| u32::from_be_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        if word(16) != self.binding {
            return Err(WireError::InvalidBinding);
        }
        let payload = usize::try_from(word(12)).map_err(|_| WireError::ArithmeticOverflow)?;
        if word(20) > word(12) {
            return Err(WireError::InvalidExtension);
        }
        let total = HEADER_BYTES
            .checked_add(payload)
            .ok_or(WireError::ArithmeticOverflow)?;
        if total > self.maximum {
            return Err(WireError::ResourceLimit);
        }
        Ok(total)
    }
    fn ready(&self) -> bool {
        self.total.is_some_and(|total| self.record.len() == total)
    }
    /// Borrow one complete frame without moving its reservation to a hidden
    /// queue. The message decoder must run before acknowledging `consume`.
    pub fn frame(&mut self, now: u64) -> Result<Option<&[u8]>, StreamError> {
        self.tick(now)?;
        Ok(self.ready().then_some(self.record.as_slice()))
    }
    pub fn consume(&mut self, now: u64) -> Result<(), StreamError> {
        self.tick(now)?;
        if !self.ready() {
            return Err(StreamError::Wire(WireError::Truncated));
        }
        self.header.fill(0);
        self.header_len = 0;
        self.record.clear();
        self.total = None;
        self.until = None;
        Ok(())
    }
    /// Call independently of packet arrival. A stalled prefix and an unconsumed
    /// complete record have the same fixed deadline; neither can live forever.
    pub fn tick(&mut self, now: u64) -> Result<(), StreamError> {
        if let Some(error) = self.failure {
            return Err(error);
        }
        if self.closed {
            return Err(StreamError::Closed);
        }
        if self.last_now.is_some_and(|old| now < old) {
            return self.fail(StreamError::ClockRegression);
        }
        self.last_now = Some(now);
        if self.until.is_some_and(|until| now >= until) {
            return self.fail(StreamError::Expired);
        }
        Ok(())
    }
    /// The caller drains a complete record before reporting peer FIN. A partial
    /// last record is a terminal framing error, never a successful stream close.
    pub fn finish(&mut self, now: u64) -> Result<(), StreamError> {
        self.tick(now)?;
        if self.header_len != 0 {
            return self.fail(StreamError::Wire(WireError::Truncated));
        }
        self.close();
        Ok(())
    }
    pub fn close(&mut self) {
        self.closed = true;
        self.header.fill(0);
        self.header_len = 0;
        self.record = Vec::new();
        self.total = None;
        self.until = None;
    }
    pub const fn next_deadline(&self) -> Option<u64> {
        self.until
    }
    pub fn allocated_bytes(&self) -> usize {
        self.record.capacity()
    }
    pub fn buffered_bytes(&self) -> usize {
        if self.total.is_some() {
            self.record.len()
        } else {
            self.header_len
        }
    }
    fn fail<T>(&mut self, error: StreamError) -> Result<T, StreamError> {
        self.close();
        self.failure = Some(error);
        Err(error)
    }
}
