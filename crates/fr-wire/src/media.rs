//! Executable media schemas, independent of codec and transport implementations.
use crate::record::{Reader, Writer};
use crate::{Kind, MediaLimits, Record, WireError};
use core::fmt;

/// Worst-case complete fragment overhead, including a predicted reference.
pub const FRAGMENT_OVERHEAD: usize = 73;
/// Complete reliable recovery-chunk overhead.
pub const RECOVERY_OVERHEAD: usize = 52;

/// Immutable declaration for one picture. `reference = None` declares an IDR;
/// the decoder boundary must independently validate that claim against HEVC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameDescriptor {
    pub frame: u64,
    pub total_bytes: u32,
    pub stride: u32,
    pub capture_micros: u64,
    pub reference: Option<u64>,
}
impl FrameDescriptor {
    pub fn validate(&self, limits: &MediaLimits) -> Result<(), WireError> {
        if self.total_bytes == 0 || self.stride == 0 {
            return Err(WireError::InvalidFragment);
        }
        if self.total_bytes > limits.protocol().max_encoded_access_unit_bytes()
            || self.stride > limits.fragment_stride()
        {
            return Err(WireError::ResourceLimit);
        }
        if self
            .reference
            .is_some_and(|reference| reference >= self.frame)
        {
            return Err(WireError::InvalidDependency);
        }
        if self.fragment_count()? > limits.max_fragments() {
            return Err(WireError::ResourceLimit);
        }
        Ok(())
    }
    pub fn fragment_count(&self) -> Result<u32, WireError> {
        if self.stride == 0 || self.total_bytes == 0 {
            return Err(WireError::InvalidFragment);
        }
        Ok(self.total_bytes.div_ceil(self.stride))
    }
    pub fn fragment_range(&self, index: u32) -> Result<core::ops::Range<usize>, WireError> {
        if index >= self.fragment_count()? {
            return Err(WireError::InvalidFragment);
        }
        let start = index
            .checked_mul(self.stride)
            .ok_or(WireError::ArithmeticOverflow)?;
        let length = self.stride.min(self.total_bytes - start);
        let end = start
            .checked_add(length)
            .ok_or(WireError::ArithmeticOverflow)?;
        Ok(
            usize::try_from(start).map_err(|_| WireError::ArithmeticOverflow)?
                ..usize::try_from(end).map_err(|_| WireError::ArithmeticOverflow)?,
        )
    }
    fn encoded_len(self) -> usize {
        25 + if self.reference.is_some() { 8 } else { 0 }
    }
    fn write(self, w: &mut Writer<'_>) -> Result<(), WireError> {
        w.u64(self.frame)?;
        w.u32(self.total_bytes)?;
        w.u32(self.stride)?;
        w.u64(self.capture_micros)?;
        w.optional_u64(self.reference)
    }
    fn read(r: &mut Reader<'_>, limits: &MediaLimits) -> Result<Self, WireError> {
        let descriptor = Self {
            frame: r.u64()?,
            total_bytes: r.u32()?,
            stride: r.u32()?,
            capture_micros: r.u64()?,
            reference: r.optional_u64()?,
        };
        descriptor.validate(limits)?;
        Ok(descriptor)
    }
}

/// A fragment borrows bytes; parsing never allocates the declared picture.
#[derive(Clone, Copy)]
pub struct Fragment<'a> {
    pub descriptor: FrameDescriptor,
    pub index: u32,
    pub bytes: &'a [u8],
}
impl fmt::Debug for Fragment<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Fragment")
            .field("descriptor", &self.descriptor)
            .field("index", &self.index)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}
impl Fragment<'_> {
    pub fn validate(&self, limits: &MediaLimits) -> Result<(), WireError> {
        self.descriptor.validate(limits)?;
        if self.descriptor.fragment_range(self.index)?.len() != self.bytes.len() {
            return Err(WireError::InvalidFragment);
        }
        Ok(())
    }
}

pub fn encode_fragment(
    fragment: Fragment<'_>,
    binding: u32,
    limits: &MediaLimits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    fragment.validate(limits)?;
    let d = fragment.descriptor;
    let payload = d
        .encoded_len()
        .checked_add(16)
        .and_then(|n| n.checked_add(fragment.bytes.len()))
        .ok_or(WireError::ArithmeticOverflow)?;
    let mut w = Writer::record(out, limits, binding, Kind::Fragment, payload)?;
    w.u64(d.frame)?;
    w.u32(d.total_bytes)?;
    w.u32(
        u32::try_from(d.fragment_range(fragment.index)?.start)
            .map_err(|_| WireError::ArithmeticOverflow)?,
    )?;
    w.u32(fragment.index)?;
    w.u32(d.fragment_count()?)?;
    w.u32(d.stride)?;
    w.u64(d.capture_micros)?;
    w.optional_u64(d.reference)?;
    w.data(fragment.bytes)?;
    w.finish()
}
pub fn decode_fragment<'a>(
    record: Record<'a>,
    limits: &MediaLimits,
) -> Result<Fragment<'a>, WireError> {
    let mut r = record.reader(Kind::Fragment)?;
    let frame = r.u64()?;
    let total_bytes = r.u32()?;
    let offset = r.u32()?;
    let index = r.u32()?;
    let count = r.u32()?;
    let stride = r.u32()?;
    let capture_micros = r.u64()?;
    let reference = r.optional_u64()?;
    let bytes = r.data()?;
    r.finish()?;
    let fragment = Fragment {
        descriptor: FrameDescriptor {
            frame,
            total_bytes,
            stride,
            capture_micros,
            reference,
        },
        index,
        bytes,
    };
    fragment.validate(limits)?;
    if count != fragment.descriptor.fragment_count()?
        || usize::try_from(offset).map_err(|_| WireError::ArithmeticOverflow)?
            != fragment.descriptor.fragment_range(index)?.start
    {
        return Err(WireError::InvalidFragment);
    }
    Ok(fragment)
}

/// Reliable recovery is contiguous chunks, not datagram fragment indexing.
#[derive(Clone, Copy)]
pub struct RecoveryChunk<'a> {
    pub frame: u64,
    pub total_bytes: u32,
    pub offset: u32,
    pub capture_micros: u64,
    pub bytes: &'a [u8],
}
impl fmt::Debug for RecoveryChunk<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryChunk")
            .field("frame", &self.frame)
            .field("total_bytes", &self.total_bytes)
            .field("offset", &self.offset)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}
impl RecoveryChunk<'_> {
    pub fn validate(&self, limits: &MediaLimits) -> Result<(), WireError> {
        if self.total_bytes == 0 || self.bytes.is_empty() {
            return Err(WireError::InvalidFragment);
        }
        if self.total_bytes > limits.protocol().max_encoded_access_unit_bytes() {
            return Err(WireError::ResourceLimit);
        }
        let length = u32::try_from(self.bytes.len()).map_err(|_| WireError::ArithmeticOverflow)?;
        if self
            .offset
            .checked_add(length)
            .ok_or(WireError::ArithmeticOverflow)?
            > self.total_bytes
        {
            return Err(WireError::InvalidFragment);
        }
        Ok(())
    }
}
pub fn encode_recovery(
    chunk: RecoveryChunk<'_>,
    binding: u32,
    limits: &MediaLimits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    chunk.validate(limits)?;
    let payload = 28_usize
        .checked_add(chunk.bytes.len())
        .ok_or(WireError::ArithmeticOverflow)?;
    let mut w = Writer::record(out, limits, binding, Kind::Recovery, payload)?;
    w.u64(chunk.frame)?;
    w.u32(chunk.total_bytes)?;
    w.u32(chunk.offset)?;
    w.u64(chunk.capture_micros)?;
    w.data(chunk.bytes)?;
    w.finish()
}
pub fn decode_recovery<'a>(
    record: Record<'a>,
    limits: &MediaLimits,
) -> Result<RecoveryChunk<'a>, WireError> {
    let mut r = record.reader(Kind::Recovery)?;
    let chunk = RecoveryChunk {
        frame: r.u64()?,
        total_bytes: r.u32()?,
        offset: r.u32()?,
        capture_micros: r.u64()?,
        bytes: r.data()?,
    };
    r.finish()?;
    chunk.validate(limits)?;
    Ok(chunk)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SourceObservation {
    Unknown = 0,
    Captured = 1,
    QualifiedUnchanged = 2,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PipelineState {
    Opening = 0,
    Running = 1,
    Idle = 2,
    Failed = 3,
}
/// Reliable announcement enables detecting loss of an entire final picture.
/// Neither a socket heartbeat nor this parsed enum independently proves freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub descriptor: FrameDescriptor,
    pub observed_micros: u64,
    pub observation: SourceObservation,
    pub pipeline: PipelineState,
}
pub fn encode_progress(
    progress: Progress,
    binding: u32,
    limits: &MediaLimits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    progress.descriptor.validate(limits)?;
    let mut w = Writer::record(
        out,
        limits,
        binding,
        Kind::Progress,
        progress.descriptor.encoded_len() + 10,
    )?;
    progress.descriptor.write(&mut w)?;
    w.u64(progress.observed_micros)?;
    w.u8(progress.observation as u8)?;
    w.u8(progress.pipeline as u8)?;
    w.finish()
}
pub fn decode_progress(record: Record<'_>, limits: &MediaLimits) -> Result<Progress, WireError> {
    let mut r = record.reader(Kind::Progress)?;
    let descriptor = FrameDescriptor::read(&mut r, limits)?;
    let observed_micros = r.u64()?;
    let observation = match r.u8()? {
        0 => SourceObservation::Unknown,
        1 => SourceObservation::Captured,
        2 => SourceObservation::QualifiedUnchanged,
        _ => return Err(WireError::InvalidValue),
    };
    let pipeline = match r.u8()? {
        0 => PipelineState::Opening,
        1 => PipelineState::Running,
        2 => PipelineState::Idle,
        3 => PipelineState::Failed,
        _ => return Err(WireError::InvalidValue),
    };
    r.finish()?;
    Ok(Progress {
        descriptor,
        observed_micros,
        observation,
        pipeline,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairRange {
    pub start: u32,
    pub end: u32,
}
fn validate_range(
    range: RepairRange,
    previous_end: u32,
    fragment_count: u32,
) -> Result<(), WireError> {
    if range.start < previous_end || range.start >= range.end || range.end > fragment_count {
        return Err(WireError::InvalidRanges);
    }
    Ok(())
}
/// The range vector stays in the caller's bounded record buffer.
#[derive(Clone, Copy)]
pub struct RepairRequest<'a> {
    pub frame: u64,
    ranges: &'a [u8],
}
impl fmt::Debug for RepairRequest<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RepairRequest")
            .field("frame", &self.frame)
            .field("range_count", &(self.ranges.len() / 8))
            .finish()
    }
}
impl RepairRequest<'_> {
    pub fn ranges(&self) -> impl Iterator<Item = RepairRange> + '_ {
        self.ranges.as_chunks::<8>().0.iter().map(|b| RepairRange {
            start: u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
            end: u32::from_be_bytes([b[4], b[5], b[6], b[7]]),
        })
    }
}
pub fn encode_repair(
    frame: u64,
    ranges: &[RepairRange],
    fragment_count: u32,
    binding: u32,
    limits: &MediaLimits,
    out: &mut [u8],
) -> Result<usize, WireError> {
    if ranges.is_empty()
        || ranges.len() > usize::from(limits.max_repair_ranges())
        || fragment_count == 0
        || fragment_count > limits.max_fragments()
    {
        return Err(WireError::InvalidRanges);
    }
    let mut previous = 0;
    for &range in ranges {
        validate_range(range, previous, fragment_count)?;
        previous = range.end;
    }
    let mut w = Writer::record(out, limits, binding, Kind::Repair, 12 + ranges.len() * 8)?;
    w.u64(frame)?;
    w.u32(u32::try_from(ranges.len()).map_err(|_| WireError::ArithmeticOverflow)?)?;
    for range in ranges {
        w.u32(range.start)?;
        w.u32(range.end)?;
    }
    w.finish()
}
pub fn decode_repair<'a>(
    record: Record<'a>,
    fragment_count: u32,
    limits: &MediaLimits,
) -> Result<RepairRequest<'a>, WireError> {
    if fragment_count == 0 || fragment_count > limits.max_fragments() {
        return Err(WireError::InvalidRanges);
    }
    let mut r = record.reader(Kind::Repair)?;
    let frame = r.u64()?;
    let count = r.u32()?;
    if count == 0 || count > u32::from(limits.max_repair_ranges()) {
        return Err(WireError::InvalidRanges);
    }
    let len = usize::try_from(count)
        .map_err(|_| WireError::ArithmeticOverflow)?
        .checked_mul(8)
        .ok_or(WireError::ArithmeticOverflow)?;
    let ranges = r.take(len)?;
    r.finish()?;
    let request = RepairRequest { frame, ranges };
    let mut previous = 0;
    for range in request.ranges() {
        validate_range(range, previous, fragment_count)?;
        previous = range.end;
    }
    Ok(request)
}
