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

pub fn receive(
    session: &mut ClipboardSession,
    bytes: &[u8],
    context: Context,
    limits: &ProtocolLimits,
    sink: &mut impl ClipboardSink,
    mut clock: impl FnMut() -> HostInstant,
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
            .begin(
                Begin {
                    binding: context.scope,
                    stamp: message.stamp,
                    total_bytes,
                    chunks,
                },
                clock(),
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
