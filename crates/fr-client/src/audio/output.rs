#![forbid(unsafe_code)]
//! Nonblocking native playback boundary supplied by a desktop client.
//! The session validates records and retains observation and transport authority.
use fr_core::audio::AudioStopReason;
use fr_wire::audio::AudioConfiguration;

/// Why no (more) audio plays; content-free and typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioEnd {
    /// The host stopped (or never started) its source, with its reason.
    Host(AudioStopReason),
    /// No local output was configured for a selected channel.
    NoOutput,
    /// The local output or its bounded reply path failed or refused.
    Local,
    /// This original optional audio pair reached its first-byte receive deadline.
    ReceiveExpired,
    /// This original optional audio pair exceeded a retained reliable-send deadline.
    SendExpired,
    /// The original audio pair was retired without a usable `AudioStop` record.
    ChannelClosed,
}

/// The local output refused or failed; audio ends, video continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputRefused;

/// Why a local output cannot be installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// The host did not select audio-down or no channel attached (typed
    /// absence: not locally enabled there, or a controlled session).
    NotSelected,
    /// An output is installed already, or the viewer already started serving.
    AlreadyConfigured,
}

/// Local playback supplied by the native client. Calls are synchronous,
/// bounded and nonblocking; `live` rechecks this session's observation at
/// each native boundary. Implementations must not log PCM or payload bytes.
/// Drop must synchronously fence further playback/decoding without waiting for
/// native completion. Any independently observed cleanup stays with its owner.
pub trait AudioOutput {
    /// Begin configuring the local output for this validated downlink offer.
    fn configure(&mut self, binding: u32, offer: AudioConfiguration) -> Result<(), OutputRefused>;
    /// Bounded progress: startup, then decode/render. Once the local output
    /// and decoder are ready, pass exactly one `AudioConfigured` record to
    /// `acknowledge`. Its `Ok` transfers the exact validated reply to this
    /// session's one bounded pending slot; do not emit it again. It is NOT
    /// transport admission or remote receipt. This owner withholds all packets
    /// until the original reply is actually admitted by the transport.
    fn service(
        &mut self,
        live: &mut dyn FnMut() -> bool,
        acknowledge: &mut dyn FnMut(&[u8]) -> Result<(), OutputRefused>,
    ) -> Result<(), OutputRefused>;
    /// The original configuration reply has actually entered transport, once.
    /// This is distinct from the callback's bounded custody and from remote
    /// receipt or audibility. Implementations may update local status here.
    fn configuration_admitted(
        &mut self,
        _binding: u32,
        _offer: AudioConfiguration,
    ) -> Result<(), OutputRefused> {
        Ok(())
    }
    /// One validated `AudioPacket` or matching `AudioStop` record.
    fn receive(
        &mut self,
        bytes: &[u8],
        live: &mut dyn FnMut() -> bool,
    ) -> Result<(), OutputRefused>;
    /// Typed end; the output stops and flushes its own device.
    fn ended(&mut self, end: AudioEnd);
    /// The local output failed (a timing miss, expired or malformed audio):
    /// drop its device and decoder NOW, without flushing, and accept one
    /// later `configure` for a strictly newer host epoch.
    fn reset(&mut self);
}
