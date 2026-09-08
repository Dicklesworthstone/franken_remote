//! The encoder/decoder send/receive state-machine contracts (plan section
//! 9.1).
//!
//! These traits model the real codec send/receive machine, not a
//! one-packet-per-frame fiction: a caller *submits* work and *drains* output
//! separately, because a submit may produce zero or several outputs and a
//! backend may signal backpressure. They are deliberately runtime-agnostic
//! (synchronous submit/poll) so the contract carries no async runtime; the
//! backend runs its blocking calls off the authority thread, and the broker
//! wraps the whole codec worker in an Asupersync region. The typed
//! [`MediaError`] distinguishes temporary backpressure, "no output yet", EOF,
//! device loss, and fatal corruption — a backend must never map device loss to
//! a sleep-retry loop.

use crate::access_unit::EncodedAccessUnit;
use crate::config::CodecConfiguration;
use crate::surface::{GpuSurface, SurfaceBackend};

/// A typed codec outcome. The distinctions are load-bearing: `Backpressure`
/// means "drain output, then retry the same input"; `NeedMoreInput` means
/// "nothing to produce yet"; `DeviceLost` and `Fatal` are terminal for the
/// session and require teardown, never a retry loop (plan section 9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MediaError {
    /// The codec cannot accept more input until output is drained.
    Backpressure,
    /// The codec has no output available yet (not an error; drives the poll
    /// loop).
    NeedMoreInput,
    /// End of stream: the codec has been flushed and will produce no more
    /// output without reconfiguration.
    EndOfStream,
    /// The codec has not been configured yet.
    NotConfigured,
    /// A surface or access unit was submitted whose configuration generation
    /// does not match the codec's current configuration (stale/fenced).
    ConfigMismatch,
    /// A surface from the wrong backend was submitted.
    WrongBackend {
        /// The backend the codec expected.
        expected: SurfaceBackend,
        /// The backend the submitted surface actually had.
        found: SurfaceBackend,
    },
    /// The underlying device was lost (GPU reset, unplug). Terminal for this
    /// session; the media profile is restarted, never sleep-retried.
    DeviceLost,
    /// Unrecoverable corruption or a foreign-library fatal error. Terminal.
    Fatal,
}

impl MediaError {
    /// True for terminal errors that require tearing the codec session down
    /// rather than retrying.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::DeviceLost | Self::Fatal)
    }
}

/// Per-submit encoder control: whether to force an IDR (startup,
/// reconfiguration, or recovery), independent of the GOP cadence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EncodeRequest {
    /// Force this input to encode as an IDR regardless of GOP position.
    pub force_idr: bool,
}

/// The encoder contract: submit capture surfaces, drain encoded access units.
/// A backend implements this over VideoToolbox, the FFmpeg bridge, or the
/// software profile; nothing here exposes a foreign codec type.
pub trait Encoder {
    /// (Re)configures the encoder. Advancing the configuration is a fence
    /// point: access units produced afterward carry the new configuration
    /// generation, and the next output MUST be an IDR (plan sections 8.1,
    /// 12.3). Returns `DeviceLost`/`Fatal` if the device is gone.
    fn configure(&mut self, config: CodecConfiguration) -> Result<(), MediaError>;

    /// Submits one capture surface for encoding. `Backpressure` means the
    /// caller must [`poll_output`](Encoder::poll_output) before retrying this
    /// same surface. A surface from the wrong backend or a mismatched
    /// configuration is a typed refusal, never encoded.
    fn submit(&mut self, surface: &dyn GpuSurface, request: EncodeRequest)
        -> Result<(), MediaError>;

    /// Drains one available encoded access unit. `NeedMoreInput` means none is
    /// ready yet. The first output after `configure` is always an IDR.
    fn poll_output(&mut self) -> Result<EncodedAccessUnit, MediaError>;

    /// The current configuration, or `None` before the first `configure`.
    fn configuration(&self) -> Option<CodecConfiguration>;
}

/// A decoded picture handle produced by a [`Decoder`]. The surface stays
/// opaque and owned by the decoder's pool until the caller releases it; the
/// caller must not assume it can be freed while a driver still references it
/// (plan section 11.2).
pub trait DecodedPicture {
    /// The frame identity this picture decodes.
    fn frame_raw(&self) -> u64;
    /// The presentation surface (opaque).
    fn surface(&self) -> &dyn GpuSurface;
}

/// The decoder contract: submit access units, drain decoded pictures. Client
/// decoders treat host-supplied access units as untrusted foreign input; the
/// bounded header validation happens before submit, and the decoder still
/// distinguishes device loss from a merely incomplete stream.
pub trait Decoder {
    /// (Re)configures the decoder from a codec configuration. The next
    /// submitted access unit MUST be an IDR for that generation.
    fn configure(&mut self, config: CodecConfiguration) -> Result<(), MediaError>;

    /// Submits one complete access unit. A unit whose configuration generation
    /// does not match the decoder's current configuration is `ConfigMismatch`
    /// and is never decoded under the wrong parameters.
    fn submit(&mut self, access_unit: &EncodedAccessUnit) -> Result<(), MediaError>;

    /// Drains one decoded picture. `NeedMoreInput` means none is ready.
    fn poll_output(&mut self) -> Result<Box<dyn DecodedPicture + '_>, MediaError>;

    /// The current configuration generation, or `None` before configuration.
    fn configuration(&self) -> Option<CodecConfiguration>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_errors_are_classified() {
        assert!(MediaError::DeviceLost.is_terminal());
        assert!(MediaError::Fatal.is_terminal());
        assert!(!MediaError::Backpressure.is_terminal());
        assert!(!MediaError::NeedMoreInput.is_terminal());
        assert!(!MediaError::ConfigMismatch.is_terminal());
    }

    #[test]
    fn encode_request_defaults_to_no_forced_idr() {
        assert!(!EncodeRequest::default().force_idr);
    }
}
