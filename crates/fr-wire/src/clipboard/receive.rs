//! Wire-to-authorized-publication bridge. A malformed ordered record fences the
//! clipboard channel and clears its buffers, without revoking unrelated input.
use super::{Body, Context, decode};
use crate::WireError;
use fr_core::{
    clipboard::{Begin, ClipboardSession, ClipboardSink, Error, Receipt},
    limits::ProtocolLimits,
    time::HostInstant,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveError {
    Wire(WireError),
    Clipboard(Error),
}

/// Canonical metadata for one complete, expired, unsubmitted ingress record.
/// Decode before dropping its payload; applying this record only releases the
/// original receiver's item or consumes its Begin sequence. It cannot publish.
#[derive(Debug, Clone, Copy)]
pub struct ExpiredRecord {
    context: Context,
    limits: ProtocolLimits,
    stamp: fr_core::clipboard::Stamp,
    begin: Option<Begin>,
}
impl ExpiredRecord {
    pub fn decode(
        bytes: &[u8],
        context: Context,
        limits: &ProtocolLimits,
    ) -> Result<Self, WireError> {
        let message = decode(bytes, context, limits)?;
        let begin = match message.body {
            Body::Begin {
                total_bytes,
                chunks,
            } => Some(Begin {
                binding: context.scope,
                stamp: message.stamp,
                total_bytes,
                chunks,
            }),
            _ => None,
        };
        Ok(Self {
            context,
            limits: *limits,
            stamp: message.stamp,
            begin,
        })
    }
    pub(super) fn apply(
        self,
        session: &mut ClipboardSession,
        context: Context,
        limits: &ProtocolLimits,
        now: HostInstant,
    ) -> Result<(), ReceiveError> {
        if self.context != context || self.limits != *limits || context.scope != session.binding() {
            session.close();
            return Err(ReceiveError::Wire(WireError::InvalidBinding));
        }
        match self.begin {
            Some(begin) => session.expire_begin(begin, now),
            None => session.expire_transfer(self.stamp, now),
        }
        .map_err(ReceiveError::Clipboard)
    }
}

pub fn receive(
    session: &mut ClipboardSession,
    bytes: &[u8],
    context: Context,
    limits: &ProtocolLimits,
    sink: &mut impl ClipboardSink,
    clock: impl FnMut() -> HostInstant,
) -> Result<Option<Receipt>, ReceiveError> {
    receive_before(
        session,
        bytes,
        context,
        limits,
        sink,
        clock,
        HostInstant::from_micros(u64::MAX),
    )
}
/// The first complete ingress record carries this fixed bound across queueing.
pub fn receive_before(
    session: &mut ClipboardSession,
    bytes: &[u8],
    context: Context,
    limits: &ProtocolLimits,
    sink: &mut impl ClipboardSink,
    mut clock: impl FnMut() -> HostInstant,
    bound: HostInstant,
) -> Result<Option<Receipt>, ReceiveError> {
    if context.scope != session.binding() {
        return Err(ReceiveError::Wire(WireError::InvalidBinding));
    }
    let message = decode(bytes, context, limits).map_err(|e| {
        session.close();
        ReceiveError::Wire(e)
    })?;
    let result = match message.body {
        Body::Begin {
            total_bytes,
            chunks,
        } => session
            .begin_before(
                Begin {
                    binding: context.scope,
                    stamp: message.stamp,
                    total_bytes,
                    chunks,
                },
                clock(),
                bound,
            )
            .map(|()| None),
        Body::Chunk {
            index,
            offset,
            bytes,
        } => session
            .chunk(message.stamp, index, offset, bytes, clock())
            .map(|()| None),
        Body::Commit { total_bytes } => session
            .commit(message.stamp, total_bytes, sink, clock)
            .map(Some),
        Body::Cancel(_) => session.cancel(message.stamp).map(|()| None),
    };
    result.map_err(ReceiveError::Clipboard)
}
