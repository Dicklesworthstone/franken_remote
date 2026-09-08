#![forbid(unsafe_code)]
//! Bounded binary media records for the `FrankenRemote` v0 protocol.
//!
//! These codecs validate framing, lengths and fragment arithmetic, not peer
//! identity, observation authority, or HEVC syntax. A transport must install
//! an admitted binding before using them. No runtime or codec is linked here.

mod media;
mod record;

pub use media::{
    FrameDescriptor, Fragment, Progress, RecoveryChunk, RepairRange, RepairRequest,
    SourceObservation, PipelineState, decode_fragment, decode_progress,
    decode_recovery, decode_repair, encode_fragment, encode_progress,
    encode_recovery, encode_repair, FRAGMENT_OVERHEAD, RECOVERY_OVERHEAD,
};
pub use record::{Channel, Kind, MediaLimits, Record, WireError, HEADER_BYTES};
