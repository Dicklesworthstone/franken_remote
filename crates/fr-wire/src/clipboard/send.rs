//! A single bounded outbound text item, encoded lazily into ordinary records.
//! No bulk record list or one-MiB control-message exception is constructed.
//! The caller checks current clipboard authority immediately before queueing
//! each returned record and calls `accepted` ONLY after the bounded transport
//! accepted that exact record. Backpressure leaves the cursor unchanged.
use super::{Body, CHUNK_OVERHEAD, Context, Message, encode};
use crate::WireError;
use core::fmt;
use fr_core::{
    clipboard::{Begin, MAX_CHUNK_BYTES, Stamp},
    limits::ProtocolLimits,
};

pub struct Sender {
    begin: Begin,
    context: Context,
    limits: ProtocolLimits,
    text: Vec<u8>,
    chunk_bytes: usize,
    step: u32,
    finished: bool,
}
impl fmt::Debug for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClipboardSender")
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}
impl Sender {
    /// `stamp` comes from the owning session's monotonic local-source sequence
    /// and qualified random transfer ID; this codec never manufactures either.
    pub fn new(
        text: &str,
        stamp: Stamp,
        context: Context,
        limits: ProtocolLimits,
    ) -> Result<Self, WireError> {
        if stamp.source != context.validate()? {
            return Err(WireError::WrongRole);
        }
        let chunk_bytes = (limits.max_control_message_bytes() as usize)
            .checked_sub(CHUNK_OVERHEAD)
            .filter(|n| *n != 0)
            .ok_or(WireError::InvalidLimits)?
            .min(MAX_CHUNK_BYTES);
        let total_bytes = u32::try_from(text.len()).map_err(|_| WireError::ResourceLimit)?;
        let chunks = u32::try_from(text.len().div_ceil(chunk_bytes))
            .map_err(|_| WireError::ResourceLimit)?;
        let begin = Begin {
            binding: context.scope,
            stamp,
            total_bytes,
            chunks,
        };
        begin
            .validate(&limits)
            .map_err(|_| WireError::ResourceLimit)?;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(text.len())
            .map_err(|_| WireError::ResourceLimit)?;
        owned.extend_from_slice(text.as_bytes());
        Ok(Self {
            begin,
            context,
            limits,
            text: owned,
            chunk_bytes,
            step: 0,
            finished: false,
        })
    }
    pub const fn stamp(&self) -> Stamp {
        self.begin.stamp
    }
    pub fn retained_bytes(&self) -> usize {
        self.text.capacity()
    }
    pub const fn is_finished(&self) -> bool {
        self.finished
    }
    /// Calling twice after backpressure yields identical bytes; nothing is sent
    /// or consumed by encoding. One record per fair runtime scheduling turn.
    pub fn encode_next(&self, out: &mut [u8]) -> Result<Option<usize>, WireError> {
        if self.finished {
            return Ok(None);
        }
        let body = if self.step == 0 {
            Body::Begin {
                total_bytes: self.begin.total_bytes,
                chunks: self.begin.chunks,
            }
        } else if self.step <= self.begin.chunks {
            let index = self.step - 1;
            let offset = index as usize * self.chunk_bytes;
            let end = (offset + self.chunk_bytes).min(self.text.len());
            Body::Chunk {
                index,
                offset: u32::try_from(offset).map_err(|_| WireError::ResourceLimit)?,
                bytes: &self.text[offset..end],
            }
        } else {
            Body::Commit {
                total_bytes: self.begin.total_bytes,
            }
        };
        encode(
            Message {
                stamp: self.begin.stamp,
                body,
            },
            self.context,
            &self.limits,
            out,
        )
        .map(Some)
    }
    /// This is transport admission, not a remote OS-publication receipt. A
    /// cancelled or failed transport must drop this item, not restart its cursor.
    pub fn accepted(&mut self) {
        if !self.finished {
            if self.step > self.begin.chunks {
                self.clear();
            } else {
                self.step += 1;
            }
        }
    }
    fn clear(&mut self) {
        self.text.fill(0);
        self.text = Vec::new();
        self.finished = true;
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.clear();
    }
}
