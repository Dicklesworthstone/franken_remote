//! Nonblocking decoder boundary for the existing single-owner playout loop.
//! A synchronous codec remains usable on its supervised native worker. A
//! process-backed decoder implements these operations without entering foreign
//! code in the caller or waiting for its child. This adds no second pipeline.
use fr_core::audio::AudioStreamConfig;
use fr_media::audio::{AudioAccessUnit, AudioDecoder, AudioMediaError, AudioPcmFrame};

pub trait PolledDecoder {
    /// Begin configuration once. No audio acknowledgement before actual readiness.
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError>;
    /// Poll the original native configuration, never wait or restart its budget.
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError>;
    /// Admit one packet. No second operation until its result is collected.
    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), AudioMediaError>;
    /// Admit one concealment operation. None means its PCM is still pending;
    /// the caller polls, never resubmits that lost packet or shifts its slot.
    fn submit_plc(&mut self, samples: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError>;
    /// Collect at most one result; None preserves the pending operation.
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError>;
}

/// Existing synchronous codecs keep their exact behavior. Such an adapter does
/// not make foreign work asynchronous; use it only on the native worker thread.
impl<D: AudioDecoder> PolledDecoder for D {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), AudioMediaError> {
        AudioDecoder::configure(self, config)
    }
    fn poll_configured(&mut self) -> Result<bool, AudioMediaError> {
        Ok(true)
    }
    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), AudioMediaError> {
        AudioDecoder::submit_packet(self, packet)
    }
    fn submit_plc(&mut self, samples: u16) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        self.decode_plc(samples).map(Some)
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, AudioMediaError> {
        AudioDecoder::poll_pcm(self)
    }
}
