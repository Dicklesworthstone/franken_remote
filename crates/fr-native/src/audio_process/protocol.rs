//! Private, bounded microphone IPC. This is never a network role or permission
//! capability. The same-boot absolute deadlines are checked by BOTH owners.
use super::{CaptureOptions, Error};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::{audio::AudioAccessUnit, worker::audio::CaptureCounters};
use std::io::{Read, Write};

pub(super) const CONFIGURE: u8 = 1;
pub(super) const PULL: u8 = 2;
const HEADER_BYTES: usize = 48;
const CONFIG_BYTES: usize = 16;
const BATCH_BYTES: usize = 12;
const PACKET_BYTES: usize = 20;
pub(super) const MAX_BODY_BYTES: usize = BATCH_BYTES
    + crate::pulse::microphone::MAX_CAPTURE_PACKETS
        * (PACKET_BYTES + fr_core::audio::MAX_OPUS_PAYLOAD_BYTES);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Header {
    pub kind: u8,
    pub serial: u64,
    pub generation: u64,
    pub authorization_ns: u64,
    pub expires_ns: u64,
    pub bytes: u32,
    pub error: Option<Error>,
}
impl Header {
    pub fn read(input: &mut impl Read, reply: bool) -> Result<Self, Error> {
        let mut b = [0; HEADER_BYTES];
        input.read_exact(&mut b).map_err(|_| Error::Native)?;
        if &b[..4] != (if reply { b"FRMR" } else { b"FRMC" })
            || b[4] != 1
            || !matches!(b[5], CONFIGURE | PULL)
            || b[44..] != [0; 4]
        {
            return Err(Error::Protocol);
        }
        let status = u16::from_be_bytes([b[6], b[7]]);
        let error = if status == 0 {
            None
        } else if reply {
            Some(match status {
                1 => Error::Configuration,
                2 => Error::Selection,
                3 => Error::Unavailable,
                4 => Error::Denied,
                5 => Error::Expired,
                6 => Error::Clock,
                7 => Error::Backpressure,
                8 => Error::Protocol,
                9 => Error::Spawn,
                10 => Error::Native,
                11 => Error::Stopped,
                12 => Error::CaptureDiscontinuity,
                13 => Error::Allocation,
                _ => return Err(Error::Protocol),
            })
        } else {
            return Err(Error::Protocol);
        };
        let word = |at| u64::from_be_bytes(b[at..at + 8].try_into().unwrap());
        let header = Self {
            kind: b[5],
            serial: word(8),
            generation: word(16),
            authorization_ns: word(24),
            expires_ns: word(32),
            bytes: u32::from_be_bytes(b[40..44].try_into().unwrap()),
            error,
        };
        if header.serial == 0
            || header.generation == 0
            || header.expires_ns == 0
            || header.expires_ns > header.authorization_ns
            || usize::try_from(header.bytes).map_or(true, |size| size > MAX_BODY_BYTES)
            || (header.error.is_some() && header.bytes != 0)
            || (!reply && header.kind == PULL && header.bytes != 0)
        {
            return Err(Error::Protocol);
        }
        Ok(header)
    }
    pub fn write(self, output: &mut impl Write, reply: bool, body: &[u8]) -> Result<(), Error> {
        if body.len() != usize::try_from(self.bytes).map_err(|_| Error::Protocol)?
            || body.len() > MAX_BODY_BYTES
        {
            return Err(Error::Protocol);
        }
        let mut b = [0; HEADER_BYTES];
        b[..4].copy_from_slice(if reply { b"FRMR" } else { b"FRMC" });
        b[4] = 1;
        b[5] = self.kind;
        b[6..8].copy_from_slice(&self.error.map_or(0, |error| error as u16).to_be_bytes());
        for (index, value) in [
            self.serial,
            self.generation,
            self.authorization_ns,
            self.expires_ns,
        ]
        .into_iter()
        .enumerate()
        {
            b[8 + index * 8..16 + index * 8].copy_from_slice(&value.to_be_bytes());
        }
        b[40..44].copy_from_slice(&self.bytes.to_be_bytes());
        output
            .write_all(&b)
            .and_then(|()| output.write_all(body))
            .map_err(|_| Error::Native)
    }
    pub fn matches_reply(self, request: Self) -> bool {
        self.kind == request.kind
            && self.serial == request.serial
            && self.generation == request.generation
            && self.authorization_ns == request.authorization_ns
            && self.expires_ns <= request.expires_ns
    }
}

/// The fixed wire cap is checked before reading any worker-selected body.
pub(super) fn body(input: &mut impl Read, header: Header) -> Result<Vec<u8>, Error> {
    let len = usize::try_from(header.bytes).map_err(|_| Error::Protocol)?;
    if len > MAX_BODY_BYTES {
        return Err(Error::Protocol);
    }
    let mut body = Vec::new();
    body.try_reserve_exact(len).map_err(|_| Error::Allocation)?;
    body.resize(len, 0);
    input.read_exact(&mut body).map_err(|_| Error::Native)?;
    Ok(body)
}

pub(super) fn configuration(options: &CaptureOptions) -> Result<Vec<u8>, Error> {
    options.validate()?;
    let server = options.server.to_str().ok_or(Error::Selection)?;
    let mut b = vec![0; CONFIG_BYTES];
    b[0] = options.configuration.channels().count();
    b[1] = u8::try_from(options.configuration.frame_duration_ms())
        .map_err(|_| Error::Configuration)?;
    b[2..4].copy_from_slice(&options.configuration.jitter_target_ms().to_be_bytes());
    b[4..8].copy_from_slice(&options.bitrate.to_be_bytes());
    b[8..10].copy_from_slice(&options.max_packet_bytes.to_be_bytes());
    b[10] = u8::try_from(server.len()).map_err(|_| Error::Selection)?;
    b[11] = u8::try_from(options.source.len()).map_err(|_| Error::Selection)?;
    b.extend_from_slice(server.as_bytes());
    b.extend_from_slice(options.source.as_bytes());
    Ok(b)
}
pub(super) fn read_configuration(header: Header, b: &[u8]) -> Result<CaptureOptions, Error> {
    if header.kind != CONFIGURE
        || header.serial != 1
        || b.len() < CONFIG_BYTES
        || b[12..16] != [0; 4]
    {
        return Err(Error::Protocol);
    }
    let server_len = usize::from(b[10]);
    let source_len = usize::from(b[11]);
    if CONFIG_BYTES + server_len + source_len != b.len() {
        return Err(Error::Protocol);
    }
    let server = std::str::from_utf8(&b[CONFIG_BYTES..CONFIG_BYTES + server_len])
        .map_err(|_| Error::Selection)?;
    let source =
        std::str::from_utf8(&b[CONFIG_BYTES + server_len..]).map_err(|_| Error::Selection)?;
    let options = CaptureOptions {
        server: server.into(),
        source: source.into(),
        configuration: AudioStreamConfig::new(
            AudioDirection::Uplink,
            AudioGeneration::from_raw(header.generation),
            AudioChannels::from_u8(b[0]).ok_or(Error::Configuration)?,
            u16::from(b[1]),
            u16::from_be_bytes([b[2], b[3]]),
        )
        .map_err(|_| Error::Configuration)?,
        bitrate: u32::from_be_bytes(b[4..8].try_into().unwrap()),
        max_packet_bytes: u16::from_be_bytes([b[8], b[9]]),
    };
    options.validate()?;
    Ok(options)
}

pub(super) fn packets(
    units: &[AudioAccessUnit],
    counters: CaptureCounters,
    options: &CaptureOptions,
) -> Result<Vec<u8>, Error> {
    if units.len() > crate::pulse::microphone::MAX_CAPTURE_PACKETS {
        return Err(Error::Protocol);
    }
    let mut bytes = BATCH_BYTES;
    for packet in units {
        if packet.direction() != AudioDirection::Uplink
            || packet.generation() != options.configuration.generation()
            || u32::from(packet.duration_samples())
                != options.configuration.expected_samples_per_frame()
            || packet.payload().len() > usize::from(options.max_packet_bytes)
        {
            return Err(Error::Protocol);
        }
        bytes += PACKET_BYTES + packet.payload().len();
    }
    let mut b = Vec::new();
    b.try_reserve_exact(bytes).map_err(|_| Error::Allocation)?;
    b.extend_from_slice(&[
        u8::try_from(units.len()).map_err(|_| Error::Protocol)?,
        0,
        0,
        0,
    ]);
    b.extend_from_slice(&counters.gaps.to_be_bytes());
    b.extend_from_slice(&counters.overruns.to_be_bytes());
    for packet in units {
        b.extend_from_slice(&packet.sequence().to_be_bytes());
        b.extend_from_slice(&packet.timestamp_samples().to_be_bytes());
        b.extend_from_slice(&packet.duration_samples().to_be_bytes());
        b.extend_from_slice(
            &u16::try_from(packet.payload().len())
                .map_err(|_| Error::Protocol)?
                .to_be_bytes(),
        );
        b.extend_from_slice(packet.payload());
    }
    Ok(b)
}

pub(super) fn read_packets(
    b: &[u8],
    options: &CaptureOptions,
) -> Result<(Vec<AudioAccessUnit>, CaptureCounters), Error> {
    if !(BATCH_BYTES..=MAX_BODY_BYTES).contains(&b.len())
        || b[1..4] != [0; 3]
        || usize::from(b[0]) > crate::pulse::microphone::MAX_CAPTURE_PACKETS
    {
        return Err(Error::Protocol);
    }
    let counters = CaptureCounters {
        gaps: u32::from_be_bytes(b[4..8].try_into().unwrap()),
        overruns: u32::from_be_bytes(b[8..12].try_into().unwrap()),
    };
    if counters.gaps != 0 || counters.overruns != 0 {
        return Err(Error::CaptureDiscontinuity);
    }
    let frames = u64::from(options.configuration.expected_samples_per_frame());
    let mut next = None;
    let mut at = BATCH_BYTES;
    // Validate the entire framing, timeline and payload bounds before allocating
    // packet storage. No trailing bytes or concealed capture gaps are accepted.
    for _ in 0..b[0] {
        if at + PACKET_BYTES > b.len() {
            return Err(Error::Protocol);
        }
        let word = |offset| u64::from_be_bytes(b[at + offset..at + offset + 8].try_into().unwrap());
        let sequence = word(0);
        let timestamp = word(8);
        let samples = u16::from_be_bytes([b[at + 16], b[at + 17]]);
        let len = usize::from(u16::from_be_bytes([b[at + 18], b[at + 19]]));
        if u64::from(samples) != frames
            || len == 0
            || len > usize::from(options.max_packet_bytes)
            || at + PACKET_BYTES + len > b.len()
            || next.is_some_and(|next| next != (sequence, timestamp))
        {
            return Err(Error::Protocol);
        }
        next = Some((
            sequence.checked_add(1).ok_or(Error::Protocol)?,
            timestamp.checked_add(frames).ok_or(Error::Protocol)?,
        ));
        at += PACKET_BYTES + len;
    }
    if at != b.len() {
        return Err(Error::Protocol);
    }
    let mut units = Vec::new();
    units
        .try_reserve_exact(usize::from(b[0]))
        .map_err(|_| Error::Allocation)?;
    at = BATCH_BYTES;
    for _ in 0..b[0] {
        let sequence = u64::from_be_bytes(b[at..at + 8].try_into().unwrap());
        let timestamp = u64::from_be_bytes(b[at + 8..at + 16].try_into().unwrap());
        let samples = u16::from_be_bytes([b[at + 16], b[at + 17]]);
        let len = usize::from(u16::from_be_bytes([b[at + 18], b[at + 19]]));
        units.push(
            AudioAccessUnit::new(
                AudioDirection::Uplink,
                options.configuration.generation(),
                sequence,
                timestamp,
                samples,
                false,
                &b[at + PACKET_BYTES..at + PACKET_BYTES + len],
            )
            .map_err(|_| Error::Protocol)?,
        );
        at += PACKET_BYTES + len;
    }
    Ok((units, counters))
}

#[cfg(test)]
#[path = "protocol/tests.rs"]
mod tests;
