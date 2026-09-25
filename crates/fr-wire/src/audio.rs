#![forbid(unsafe_code)]
//! Allocation-free Opus audio wire serialization and deserialization.
//!
//! Conforms to plan section 15.4 and PROTOCOL.md section 17:
//! - 0x0060: `AudioConfiguration`
//! - 0x0061: `AudioConfigured`
//! - 0x0062: `AudioPacket`
//! - 0x0063: `AudioStop`
//!
//! Audio packets and parameters are strictly validated before any buffer allocation.

use crate::{HEADER_BYTES, Kind, Record, WireError, record::Writer};
use fr_core::{
    audio::{
        AudioChannels, AudioDirection, AudioStopReason, MAX_DECODED_SAMPLES, MAX_JITTER_CEILING_MS,
        MAX_OPUS_PAYLOAD_BYTES, MAX_PACKET_DURATION_MS, OPUS_SAMPLE_RATE,
    },
    ids::AudioGeneration,
};

/// Optional host-playback downlink (plan §15.4; PROTOCOL.md `audio-down`).
/// Positive selection by BOTH peers attaches one `MediaRole::AudioDown`
/// channel: reliable `AudioConfiguration`/`AudioStop` host-to-viewer,
/// `AudioConfigured`/`AudioStop` viewer-to-host, and `AudioPacket` datagrams.
/// Selection is never the host's local enable, never observation approval and
/// never a microphone (uplink) grant; a peer that did not select it gets no
/// audio record and nothing else changes.
pub const CAPABILITY: &str = "native-audio-down";
pub const VERSION: u16 = 1;

pub const AUDIO_CONFIGURATION_PAYLOAD_BYTES: usize = 28;
pub const AUDIO_CONFIGURATION_RECORD_BYTES: usize =
    HEADER_BYTES + AUDIO_CONFIGURATION_PAYLOAD_BYTES;

pub const AUDIO_CONFIGURED_PAYLOAD_BYTES: usize = 20;
pub const AUDIO_CONFIGURED_RECORD_BYTES: usize = HEADER_BYTES + AUDIO_CONFIGURED_PAYLOAD_BYTES;

pub const AUDIO_PACKET_HEADER_BYTES: usize = 32;
pub const AUDIO_PACKET_OVERHEAD: usize = HEADER_BYTES + AUDIO_PACKET_HEADER_BYTES;

pub const AUDIO_STOP_PAYLOAD_BYTES: usize = 16;
pub const AUDIO_STOP_RECORD_BYTES: usize = HEADER_BYTES + AUDIO_STOP_PAYLOAD_BYTES;

/// Complete `AudioPacket` record bytes for an Opus payload of `payload` bytes.
/// `None` when the payload is empty or above the absolute Opus bound.
pub const fn packet_record_bytes(payload: usize) -> Option<usize> {
    if payload == 0 || payload > MAX_OPUS_PAYLOAD_BYTES {
        None
    } else {
        Some(AUDIO_PACKET_OVERHEAD + payload)
    }
}

/// Record kind of a complete FRD0 record, read from its fixed header without
/// validating or allocating anything. Callers still fully decode the record.
pub fn record_kind(bytes: &[u8]) -> Option<u16> {
    bytes.get(6..8).map(|k| u16::from_be_bytes([k[0], k[1]]))
}

/// Stream configuration offer / request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfiguration {
    pub direction: AudioDirection,
    pub generation: AudioGeneration,
    pub channels: AudioChannels,
    pub sample_rate: u32,
    pub frame_duration_ms: u16,
    pub max_packet_bytes: u32,
    pub max_decoded_samples: u32,
    pub jitter_target_ms: u16,
}

impl AudioConfiguration {
    pub fn validate(&self) -> Result<(), WireError> {
        if self.sample_rate != OPUS_SAMPLE_RATE {
            return Err(WireError::InvalidValue);
        }
        if self.frame_duration_ms == 0 || self.frame_duration_ms > MAX_PACKET_DURATION_MS {
            return Err(WireError::InvalidValue);
        }
        if self.max_packet_bytes == 0 || (self.max_packet_bytes as usize) > MAX_OPUS_PAYLOAD_BYTES {
            return Err(WireError::ResourceLimit);
        }
        if self.max_decoded_samples == 0 || self.max_decoded_samples > MAX_DECODED_SAMPLES {
            return Err(WireError::ResourceLimit);
        }
        if self.jitter_target_ms == 0 || self.jitter_target_ms > MAX_JITTER_CEILING_MS {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}

/// Acknowledged audio stream configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfigured {
    pub direction: AudioDirection,
    pub generation: AudioGeneration,
    pub accepted: bool,
    pub actual_channels: AudioChannels,
    pub actual_sample_rate: u32,
    pub actual_frame_duration_ms: u16,
}

impl AudioConfigured {
    pub fn validate(&self) -> Result<(), WireError> {
        if self.accepted {
            if self.actual_sample_rate != OPUS_SAMPLE_RATE {
                return Err(WireError::InvalidValue);
            }
            if self.actual_frame_duration_ms == 0
                || self.actual_frame_duration_ms > MAX_PACKET_DURATION_MS
            {
                return Err(WireError::InvalidValue);
            }
        }
        Ok(())
    }
}

/// Opus-encoded audio access unit packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    pub direction: AudioDirection,
    pub generation: AudioGeneration,
    pub sequence: u64,
    pub timestamp_samples: u64,
    pub duration_samples: u16,
    pub payload: &'a [u8],
}

impl AudioPacket<'_> {
    pub fn validate(&self) -> Result<(), WireError> {
        if self.duration_samples == 0
            || u32::from(self.duration_samples) > MAX_DECODED_SAMPLES
            || self.payload.is_empty()
            || self.payload.len() > MAX_OPUS_PAYLOAD_BYTES
        {
            return Err(WireError::InvalidValue);
        }
        Ok(())
    }
}

/// Stream termination notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioStop {
    pub direction: AudioDirection,
    pub generation: AudioGeneration,
    pub reason: AudioStopReason,
}

// Encoding implementations

pub fn encode_configuration(
    cfg: &AudioConfiguration,
    binding: u32,
    out: &mut [u8],
) -> Result<usize, WireError> {
    cfg.validate()?;
    let mut w = Writer::record_bounded(
        out,
        AUDIO_CONFIGURATION_RECORD_BYTES,
        binding,
        Kind::AudioConfiguration,
        AUDIO_CONFIGURATION_PAYLOAD_BYTES,
    )?;
    w.u8(cfg.direction as u8)?;
    w.u8(cfg.channels as u8)?;
    w.u16(cfg.frame_duration_ms)?;
    w.u16(cfg.jitter_target_ms)?;
    w.u16(0)?; // reserved
    w.u32(cfg.sample_rate)?;
    w.u32(cfg.max_packet_bytes)?;
    w.u32(cfg.max_decoded_samples)?;
    w.u64(cfg.generation.as_raw())?;
    w.finish()
}

pub fn decode_configuration(bytes: &[u8], binding: u32) -> Result<AudioConfiguration, WireError> {
    let rec = Record::decode_bounded(bytes, AUDIO_CONFIGURATION_RECORD_BYTES, binding, None)?;
    if rec.kind() != Kind::AudioConfiguration {
        return Err(WireError::UnsupportedKind);
    }
    let mut r = rec.reader(Kind::AudioConfiguration)?;
    let direction_byte = r.u8()?;
    let direction = AudioDirection::from_u8(direction_byte).ok_or(WireError::InvalidValue)?;
    let channels_byte = r.u8()?;
    let channels = AudioChannels::from_u8(channels_byte).ok_or(WireError::InvalidValue)?;
    let frame_duration_ms = r.u16()?;
    let jitter_target_ms = r.u16()?;
    let _reserved = r.u16()?;
    let sample_rate = r.u32()?;
    let max_packet_bytes = r.u32()?;
    let max_decoded_samples = r.u32()?;
    let generation_raw = r.u64()?;
    r.finish()?;

    let cfg = AudioConfiguration {
        direction,
        generation: AudioGeneration::from_raw(generation_raw),
        channels,
        sample_rate,
        frame_duration_ms,
        max_packet_bytes,
        max_decoded_samples,
        jitter_target_ms,
    };
    cfg.validate()?;
    Ok(cfg)
}

pub fn encode_configured(
    ack: &AudioConfigured,
    binding: u32,
    out: &mut [u8],
) -> Result<usize, WireError> {
    ack.validate()?;
    let mut w = Writer::record_bounded(
        out,
        AUDIO_CONFIGURED_RECORD_BYTES,
        binding,
        Kind::AudioConfigured,
        AUDIO_CONFIGURED_PAYLOAD_BYTES,
    )?;
    w.u8(ack.direction as u8)?;
    w.u8(u8::from(ack.accepted))?;
    w.u8(ack.actual_channels as u8)?;
    w.u8(0)?; // reserved
    w.u32(ack.actual_sample_rate)?;
    w.u16(ack.actual_frame_duration_ms)?;
    w.u16(0)?; // reserved
    w.u64(ack.generation.as_raw())?;
    w.finish()
}

pub fn decode_configured(bytes: &[u8], binding: u32) -> Result<AudioConfigured, WireError> {
    let rec = Record::decode_bounded(bytes, AUDIO_CONFIGURED_RECORD_BYTES, binding, None)?;
    if rec.kind() != Kind::AudioConfigured {
        return Err(WireError::UnsupportedKind);
    }
    let mut r = rec.reader(Kind::AudioConfigured)?;
    let direction_byte = r.u8()?;
    let direction = AudioDirection::from_u8(direction_byte).ok_or(WireError::InvalidValue)?;
    let accepted_byte = r.u8()?;
    let accepted = match accepted_byte {
        0 => false,
        1 => true,
        _ => return Err(WireError::InvalidValue),
    };
    let channels_byte = r.u8()?;
    let actual_channels = AudioChannels::from_u8(channels_byte).ok_or(WireError::InvalidValue)?;
    let _reserved = r.u8()?;
    let actual_sample_rate = r.u32()?;
    let actual_frame_duration_ms = r.u16()?;
    let _reserved2 = r.u16()?;
    let generation_raw = r.u64()?;
    r.finish()?;

    let ack = AudioConfigured {
        direction,
        generation: AudioGeneration::from_raw(generation_raw),
        accepted,
        actual_channels,
        actual_sample_rate,
        actual_frame_duration_ms,
    };
    ack.validate()?;
    Ok(ack)
}

pub fn encode_packet(
    packet: &AudioPacket<'_>,
    binding: u32,
    out: &mut [u8],
) -> Result<usize, WireError> {
    packet.validate()?;
    let payload_len = packet.payload.len();
    let total_payload = AUDIO_PACKET_HEADER_BYTES
        .checked_add(payload_len)
        .ok_or(WireError::ArithmeticOverflow)?;
    let max_record = AUDIO_PACKET_OVERHEAD
        .checked_add(payload_len)
        .ok_or(WireError::ArithmeticOverflow)?;

    let mut w = Writer::record_bounded(out, max_record, binding, Kind::AudioPacket, total_payload)?;
    w.u8(packet.direction as u8)?;
    w.u8(0)?; // reserved
    w.u16(packet.duration_samples)?;
    w.u32(u32::try_from(payload_len).map_err(|_| WireError::ArithmeticOverflow)?)?;
    w.u64(packet.generation.as_raw())?;
    w.u64(packet.sequence)?;
    w.u64(packet.timestamp_samples)?;
    w.put(packet.payload)?;
    w.finish()
}

pub fn decode_packet(bytes: &[u8], binding: u32) -> Result<AudioPacket<'_>, WireError> {
    let rec = Record::decode_bounded(
        bytes,
        AUDIO_PACKET_OVERHEAD + MAX_OPUS_PAYLOAD_BYTES,
        binding,
        None,
    )?;
    if rec.kind() != Kind::AudioPacket {
        return Err(WireError::UnsupportedKind);
    }
    let mut r = rec.reader(Kind::AudioPacket)?;
    let direction_byte = r.u8()?;
    let direction = AudioDirection::from_u8(direction_byte).ok_or(WireError::InvalidValue)?;
    let _reserved = r.u8()?;
    let duration_samples = r.u16()?;
    let payload_len = usize::try_from(r.u32()?).map_err(|_| WireError::ArithmeticOverflow)?;
    let generation_raw = r.u64()?;
    let sequence = r.u64()?;
    let timestamp_samples = r.u64()?;
    let payload = r.take(payload_len)?;
    r.finish()?;

    let packet = AudioPacket {
        direction,
        generation: AudioGeneration::from_raw(generation_raw),
        sequence,
        timestamp_samples,
        duration_samples,
        payload,
    };
    packet.validate()?;
    Ok(packet)
}

pub fn encode_stop(stop: &AudioStop, binding: u32, out: &mut [u8]) -> Result<usize, WireError> {
    let mut w = Writer::record_bounded(
        out,
        AUDIO_STOP_RECORD_BYTES,
        binding,
        Kind::AudioStop,
        AUDIO_STOP_PAYLOAD_BYTES,
    )?;
    w.u8(stop.direction as u8)?;
    w.u8(stop.reason as u8)?;
    w.u16(0)?; // reserved
    w.u32(0)?; // reserved2
    w.u64(stop.generation.as_raw())?;
    w.finish()
}

pub fn decode_stop(bytes: &[u8], binding: u32) -> Result<AudioStop, WireError> {
    let rec = Record::decode_bounded(bytes, AUDIO_STOP_RECORD_BYTES, binding, None)?;
    if rec.kind() != Kind::AudioStop {
        return Err(WireError::UnsupportedKind);
    }
    let mut r = rec.reader(Kind::AudioStop)?;
    let direction_byte = r.u8()?;
    let direction = AudioDirection::from_u8(direction_byte).ok_or(WireError::InvalidValue)?;
    let reason_byte = r.u8()?;
    let reason = AudioStopReason::from_u8(reason_byte).ok_or(WireError::InvalidValue)?;
    let _reserved = r.u16()?;
    let _reserved2 = r.u32()?;
    let generation_raw = r.u64()?;
    r.finish()?;

    Ok(AudioStop {
        direction,
        generation: AudioGeneration::from_raw(generation_raw),
        reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_configuration_round_trip() {
        let cfg = AudioConfiguration {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::INITIAL,
            channels: AudioChannels::Stereo,
            sample_rate: 48_000,
            frame_duration_ms: 10,
            max_packet_bytes: 1275,
            max_decoded_samples: 5760,
            jitter_target_ms: 20,
        };

        let mut buf = [0u8; 128];
        let len = encode_configuration(&cfg, 42, &mut buf).unwrap();
        assert_eq!(len, AUDIO_CONFIGURATION_RECORD_BYTES);

        let decoded = decode_configuration(&buf[..len], 42).unwrap();
        assert_eq!(decoded, cfg);

        // Binding mismatch refuses
        assert_eq!(
            decode_configuration(&buf[..len], 99),
            Err(WireError::InvalidBinding)
        );
    }

    #[test]
    fn audio_configured_round_trip() {
        let ack = AudioConfigured {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::INITIAL,
            accepted: true,
            actual_channels: AudioChannels::Stereo,
            actual_sample_rate: 48_000,
            actual_frame_duration_ms: 10,
        };

        let mut buf = [0u8; 128];
        let len = encode_configured(&ack, 42, &mut buf).unwrap();
        assert_eq!(len, AUDIO_CONFIGURED_RECORD_BYTES);

        let decoded = decode_configured(&buf[..len], 42).unwrap();
        assert_eq!(decoded, ack);
    }

    #[test]
    fn audio_packet_round_trip() {
        let opus_bytes = [0x78, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc];
        let packet = AudioPacket {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::INITIAL,
            sequence: 1,
            timestamp_samples: 480,
            duration_samples: 480,
            payload: &opus_bytes,
        };

        let mut buf = [0u8; 256];
        let len = encode_packet(&packet, 42, &mut buf).unwrap();
        assert_eq!(len, AUDIO_PACKET_OVERHEAD + opus_bytes.len());

        let decoded = decode_packet(&buf[..len], 42).unwrap();
        assert_eq!(decoded, packet);
    }

    #[test]
    fn audio_stop_round_trip() {
        let stop = AudioStop {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::INITIAL,
            reason: AudioStopReason::UserMute,
        };

        let mut buf = [0u8; 128];
        let len = encode_stop(&stop, 42, &mut buf).unwrap();
        assert_eq!(len, AUDIO_STOP_RECORD_BYTES);

        let decoded = decode_stop(&buf[..len], 42).unwrap();
        assert_eq!(decoded, stop);
    }

    #[test]
    fn hostile_packet_records_refuse_before_any_payload_use() {
        let opus = [0x78_u8; 40];
        let packet = AudioPacket {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(3),
            sequence: 9,
            timestamp_samples: 960,
            duration_samples: 960,
            payload: &opus,
        };
        let mut buf = [0u8; 256];
        let len = encode_packet(&packet, 42, &mut buf).unwrap();
        // A declared Opus length larger than the record body cannot borrow
        // past the record, and a smaller one leaves refused trailing bytes.
        for declared in [41_u32, 40_000, u32::MAX, 39] {
            let mut forged = buf[..len].to_vec();
            forged[HEADER_BYTES + 4..HEADER_BYTES + 8].copy_from_slice(&declared.to_be_bytes());
            assert!(decode_packet(&forged, 42).is_err(), "{declared}");
        }
        // Oversized complete records refuse on length alone, before parsing.
        let oversized = vec![0_u8; AUDIO_PACKET_OVERHEAD + MAX_OPUS_PAYLOAD_BYTES + 1];
        assert_eq!(decode_packet(&oversized, 42), Err(WireError::ResourceLimit));
        // Zero or oversized decoded-sample claims refuse.
        for duration in [0_u16, u16::MAX] {
            let mut forged = buf[..len].to_vec();
            forged[HEADER_BYTES + 2..HEADER_BYTES + 4].copy_from_slice(&duration.to_be_bytes());
            assert!(decode_packet(&forged, 42).is_err(), "{duration}");
        }
        // Wrong binding and every truncation refuse.
        assert_eq!(
            decode_packet(&buf[..len], 7),
            Err(WireError::InvalidBinding)
        );
        for end in 0..len {
            assert!(decode_packet(&buf[..end], 42).is_err());
        }
        assert_eq!(packet_record_bytes(0), None);
        assert_eq!(packet_record_bytes(MAX_OPUS_PAYLOAD_BYTES + 1), None);
        assert_eq!(packet_record_bytes(40), Some(len));
        assert_eq!(record_kind(&buf[..len]), Some(Kind::AudioPacket as u16));
    }

    #[test]
    fn hostile_configuration_budgets_refuse() {
        let cfg = AudioConfiguration {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::from_raw(4),
            channels: AudioChannels::Stereo,
            sample_rate: 48_000,
            frame_duration_ms: 20,
            max_packet_bytes: 1000,
            max_decoded_samples: 960,
            jitter_target_ms: 40,
        };
        let mut buf = [0u8; 128];
        let len = encode_configuration(&cfg, 42, &mut buf).unwrap();
        let payload = HEADER_BYTES;
        // sample rate (offset 8), max_packet_bytes (12), max_decoded_samples (16).
        for (offset, value, error) in [
            (12, 1276_u32, WireError::ResourceLimit),
            (12, 0, WireError::ResourceLimit),
            (16, MAX_DECODED_SAMPLES + 1, WireError::ResourceLimit),
            (8, 44_100, WireError::InvalidValue),
        ] {
            let mut forged = buf[..len].to_vec();
            forged[payload + offset..payload + offset + 4].copy_from_slice(&value.to_be_bytes());
            assert_eq!(decode_configuration(&forged, 42), Err(error), "{offset}");
        }
        let mut forged = buf[..len].to_vec();
        forged[payload + 1] = 3; // three channels
        assert_eq!(
            decode_configuration(&forged, 42),
            Err(WireError::InvalidValue)
        );
        let mut ack = [0u8; 64];
        let n = encode_configured(
            &AudioConfigured {
                direction: AudioDirection::Downlink,
                generation: AudioGeneration::from_raw(4),
                accepted: true,
                actual_channels: AudioChannels::Stereo,
                actual_sample_rate: 48_000,
                actual_frame_duration_ms: 20,
            },
            42,
            &mut ack,
        )
        .unwrap();
        ack[HEADER_BYTES + 1] = 2; // neither accepted nor refused
        assert_eq!(
            decode_configured(&ack[..n], 42),
            Err(WireError::InvalidValue)
        );
    }

    #[test]
    fn invalid_bounds_refuse_encoding() {
        let mut buf = [0u8; 128];

        // Zero duration refuses
        let bad_cfg = AudioConfiguration {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::INITIAL,
            channels: AudioChannels::Stereo,
            sample_rate: 48_000,
            frame_duration_ms: 0,
            max_packet_bytes: 1275,
            max_decoded_samples: 5760,
            jitter_target_ms: 20,
        };
        assert_eq!(
            encode_configuration(&bad_cfg, 42, &mut buf),
            Err(WireError::InvalidValue)
        );

        // Empty audio payload refuses
        let bad_packet = AudioPacket {
            direction: AudioDirection::Downlink,
            generation: AudioGeneration::INITIAL,
            sequence: 1,
            timestamp_samples: 0,
            duration_samples: 480,
            payload: &[],
        };
        assert_eq!(
            encode_packet(&bad_packet, 42, &mut buf),
            Err(WireError::InvalidValue)
        );
    }
}
