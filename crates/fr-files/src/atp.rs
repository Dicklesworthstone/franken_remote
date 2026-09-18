//! Pinned Asupersync 0.5.0 ATP reliable single-entry data profile.
//!
//! Uses the upstream frame codec and the existing `transport_tcp` `ObjectData`
//! layout: entry index (u32 BE, zero here), offset (u64 BE), then bytes. The
//! sender terminates with an empty `ObjectComplete`. No new hashing, chunk repair,
//! identity, or resumption protocol is introduced. Unsupported frames refuse.
use crate::{receive::MAX_CHUNK_BYTES, session::Error};
use asupersync::{
    bytes::BytesMut,
    codec::{Decoder, Encoder},
    net::atp::protocol::{AtpFrameCodec, Frame, FrameType, ProtocolVersion},
};
pub const MAX_FRAME_BYTES: usize = MAX_CHUNK_BYTES + 32;
pub struct ObjectRecord(Frame);
impl std::fmt::Debug for ObjectRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AtpObjectRecord([private])")
    }
}
impl Drop for ObjectRecord {
    fn drop(&mut self) {
        self.0.payload.fill(0);
    }
}
impl ObjectRecord {
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
            return Err(Error::Protocol);
        }
        let mut buffer = BytesMut::from(bytes);
        let result = AtpFrameCodec::with_max_frame_size(MAX_FRAME_BYTES as u64)
            .decode(&mut buffer)
            .map_err(|_| Error::Protocol)?;
        let record = Self(result.ok_or(Error::Protocol)?);
        if !buffer.is_empty() || !record.0.header.extensions.is_empty() {
            return Err(Error::Protocol);
        }
        match record.0.frame_type() {
            FrameType::ObjectData => {
                let p = record.0.payload();
                if p.len() <= 12 || p.len() > MAX_CHUNK_BYTES + 12 || p[..4] != [0; 4] {
                    return Err(Error::Protocol);
                }
            }
            FrameType::ObjectComplete if record.0.payload.is_empty() => {}
            _ => return Err(Error::Protocol),
        }
        Ok(record)
    }
    pub fn data(&self) -> Option<(u64, &[u8])> {
        if self.0.frame_type() != FrameType::ObjectData {
            return None;
        }
        let p = self.0.payload();
        Some((
            u64::from_be_bytes(p[4..12].try_into().expect("validated ATP offset")),
            &p[12..],
        ))
    }
}
/// Encode using the upstream ATP codec, not a parallel frame implementation.
pub fn encode_data(offset: u64, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.is_empty() || bytes.len() > MAX_CHUNK_BYTES {
        return Err(Error::Protocol);
    }
    let mut payload = Vec::with_capacity(bytes.len() + 12);
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&offset.to_be_bytes());
    payload.extend_from_slice(bytes);
    encode(
        Frame::new(ProtocolVersion::V0, FrameType::ObjectData, payload)
            .map_err(|_| Error::Protocol)?,
    )
}
pub fn encode_complete() -> Result<Vec<u8>, Error> {
    encode(Frame::empty(FrameType::ObjectComplete).map_err(|_| Error::Protocol)?)
}
fn encode(frame: Frame) -> Result<Vec<u8>, Error> {
    let mut out = BytesMut::new();
    AtpFrameCodec::with_max_frame_size(MAX_FRAME_BYTES as u64)
        .encode(frame, &mut out)
        .map_err(|_| Error::Protocol)?;
    Ok(out.to_vec())
}

/// Full-object manifests and verified replies on the same disk owner.
pub mod receive;
