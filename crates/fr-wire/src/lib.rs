#![forbid(unsafe_code)]
//! Bounded binary media records for the `FrankenRemote` v0 protocol.
//!
//! These codecs validate framing, lengths and fragment arithmetic, not peer
//! identity, observation authority, or HEVC syntax. A transport must install
//! an admitted binding before using them. No runtime or codec is linked here.

pub mod attachment;
pub mod authority;
pub mod clock;
pub mod decoder;
pub mod display;
pub mod held_state;
pub mod input;
pub mod input_result;
mod media;
pub mod negotiation;
pub mod receiver_metrics;
mod record;
pub mod stream;

pub use media::{
    FRAGMENT_OVERHEAD, Fragment, FrameDescriptor, PipelineState, Progress, RECOVERY_OVERHEAD,
    RecoveryChunk, RepairRange, RepairRequest, SourceObservation, decode_fragment, decode_progress,
    decode_recovery, decode_repair, encode_fragment, encode_progress, encode_recovery,
    encode_repair,
};
pub use record::{
    Channel, HEADER_BYTES, Kind, MAX_FRAGMENTS, MAX_REPAIR_RANGES, MediaLimits, Record, WireError,
};

pub mod input_ticket;

/// Initial control requests and locally approved grants.
pub mod control;
