//! Bound FRD0 audio records to the actual Opus decoder and paced client owner.
//! This is not session admission, device configuration or an `AudioConfigured`
//! acknowledgement. Construct once on the audio worker AFTER those succeed.

use super::{CodecLimits, Decoder};
use fr_client::audio::{
    AudioVolumeControl,
    playout::{AudioPlayout, AudioSubmission, PlayoutClock, PlayoutError, PlayoutResult},
};
use fr_core::audio::{AudioStopReason, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioMediaError, AudioPcmFrame};
use fr_wire::{
    WireError,
    audio::{self, AudioConfiguration},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Binding,
    Configuration,
    Wire(WireError),
    Media(AudioMediaError),
    Playout(PlayoutError),
}
impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "native-audio-playout: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiveResult {
    Queued,
    Ignored,
    Stopped(AudioStopReason),
}

/// One admitted channel binding, direction and epoch. !Send/!Sync because the
/// real native decoder is thread-confined. No PCM/packet dump, decoder escape,
/// dynamic library path, implicit microphone enable or output-device claim.
/// On stop/reconnect, retire this owner and negotiate a strictly newer epoch in
/// the containing session; remote records cannot reconfigure or reopen it.
pub struct OpusPlayout {
    binding: u32,
    offer: AudioConfiguration,
    owner: AudioPlayout<Decoder>,
}
impl OpusPlayout {
    pub fn new(
        binding: u32,
        offer: AudioConfiguration,
        clock: PlayoutClock,
    ) -> Result<Self, Error> {
        if binding == 0 {
            return Err(Error::Binding);
        }
        offer.validate().map_err(Error::Wire)?;
        let config = AudioStreamConfig::new(
            offer.direction,
            offer.generation,
            offer.channels,
            offer.frame_duration_ms,
            offer.jitter_target_ms,
        )
        .map_err(|_| Error::Configuration)?;
        // Cross-field budget checks happen before the first native allocation.
        if config.expected_samples_per_frame() > offer.max_decoded_samples {
            return Err(Error::Configuration);
        }
        let limits = CodecLimits::new(
            usize::try_from(offer.max_packet_bytes).map_err(|_| Error::Configuration)?,
            offer.max_decoded_samples,
        )
        .map_err(Error::Media)?;
        let owner = AudioPlayout::new(config, Decoder::with_limits(limits), clock)
            .map_err(Error::Playout)?;
        Ok(Self {
            binding,
            offer,
            owner,
        })
    }
    pub const fn configuration(&self) -> AudioConfiguration {
        self.offer
    }
    pub const fn error(&self) -> Option<PlayoutError> {
        self.owner.error()
    }
    pub const fn queued_packets(&self) -> usize {
        self.owner.queued_packets()
    }
    pub fn volume_mut(&mut self) -> &mut AudioVolumeControl {
        self.owner.volume_mut()
    }
    pub fn stop(&mut self) {
        self.owner.stop();
    }

    /// Complete, bounded records from the ORIGINAL authenticated audio channel.
    /// Binding is checked by the wire decoder, not copied from peer metadata.
    /// Narrower negotiated byte/sample limits apply before copying Opus bytes
    /// into the jitter queue, as well as inside the actual codec. Parse/resource
    /// refusals do not refresh deadlines or fabricate a configuration grant.
    pub fn receive_record(
        &mut self,
        bytes: &[u8],
        clock: PlayoutClock,
    ) -> Result<ReceiveResult, Error> {
        let packet = match audio::decode_packet(bytes, self.binding) {
            Ok(packet) => packet,
            Err(WireError::UnsupportedKind) => {
                let stop = audio::decode_stop(bytes, self.binding).map_err(Error::Wire)?;
                if stop.direction != self.offer.direction
                    || stop.generation != self.offer.generation
                {
                    return Ok(ReceiveResult::Ignored);
                }
                self.owner.stop();
                return Ok(ReceiveResult::Stopped(stop.reason));
            }
            Err(error) => return Err(Error::Wire(error)),
        };
        if packet.direction != self.offer.direction || packet.generation != self.offer.generation {
            return Ok(ReceiveResult::Ignored);
        }
        if packet.payload.len()
            > usize::try_from(self.offer.max_packet_bytes).map_err(|_| Error::Configuration)?
            || u32::from(packet.duration_samples) > self.offer.max_decoded_samples
        {
            return Err(Error::Wire(WireError::ResourceLimit));
        }
        let packet = AudioAccessUnit::new(
            packet.direction,
            packet.generation,
            packet.sequence,
            packet.timestamp_samples,
            packet.duration_samples,
            false,
            packet.payload,
        )
        .map_err(Error::Media)?;
        let queued = self.owner.receive(packet, clock).map_err(Error::Playout)?;
        Ok(if queued {
            ReceiveResult::Queued
        } else {
            ReceiveResult::Ignored
        })
    }

    /// See `AudioPlayout::render` for checkpoint and actual OS submission duties.
    /// The adapter must retire/flush its own device queue on stop/revocation.
    pub fn render(
        &mut self,
        checkpoint: impl FnMut() -> Result<PlayoutClock, PlayoutError>,
        submit: impl FnOnce(&AudioPcmFrame, AudioSubmission) -> Result<(), PlayoutError>,
    ) -> Result<PlayoutResult, Error> {
        self.owner
            .render(checkpoint, submit)
            .map_err(Error::Playout)
    }
}
