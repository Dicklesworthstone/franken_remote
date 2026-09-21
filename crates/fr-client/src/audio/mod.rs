#![forbid(unsafe_code)]
//! Client audio management, jitter buffer, volume control, and A/V sync.
//!
//! Conforms to plan section 15.4:
//! - Small bounded adaptive jitter buffer with strict ceiling (`MAX_JITTER_CEILING_MS`).
//! - Packet loss concealment (PLC) without synthetic video delays.
//! - Local-only volume control and instant mute (zero host round trips) with per-host persistence.
//! - Generational resets preventing stale audio playback across reconnects or device switches.
//! - Strict A/V sync bounds: fresh video is never held behind delayed audio.

pub mod jitter;
pub mod sync;
pub mod volume;

pub use jitter::{AudioJitterBuffer, JitterBufferMetrics, JitterDrainResult};
pub use sync::{AudioVideoSyncController, AvAlignment};
pub use volume::{AudioVolumeControl, HostAudioSettings, HostAudioStore};
