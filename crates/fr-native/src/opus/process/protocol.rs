//! Private bounded stream codec. Not a remote protocol or authority capability.
use super::{AudioMediaError as Error, CodecLimits};
use fr_core::audio::{AudioChannels, AudioDirection, AudioGeneration, AudioStreamConfig};
use fr_media::audio::{AudioAccessUnit, AudioPcmFrame, MAX_PCM_BUFFER_SAMPLES};
use std::io::{Read, Write};
pub(super) const HEADER: usize = 48;
pub(super) const CONFIG: u8 = 1;
pub(super) const PACKET: u8 = 2;
pub(super) const PLC: u8 = 3;
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Header {
    pub kind: u8,
    pub serial: u64,
    pub generation: u64,
    pub at: u64,
    pub sequence: u64,
    pub bytes: u32,
    pub samples: u16,
    pub channels: u8,
    pub direction: u8,
}
impl Header {
    pub fn new(config: AudioStreamConfig, serial: u64, kind: u8) -> Result<Self, Error> {
        Ok(Self {
            kind,
            serial,
            generation: config.generation().as_raw(),
            at: 0,
            sequence: 0,
            bytes: 0,
            samples: u16::try_from(config.expected_samples_per_frame())
                .map_err(|_| Error::BufferOverflow)?,
            channels: config.channels().count(),
            direction: u8::from(config.direction() == AudioDirection::Uplink),
        })
    }
    pub fn encode(self, reply: bool) -> [u8; HEADER] {
        let mut b = [0; HEADER];
        b[..4].copy_from_slice(if reply { b"FROR" } else { b"FROP" });
        b[4] = 1;
        b[5] = self.kind;
        for (i, n) in [self.serial, self.generation, self.at, self.sequence]
            .into_iter()
            .enumerate()
        {
            b[8 + i * 8..16 + i * 8].copy_from_slice(&n.to_be_bytes());
        }
        b[40..44].copy_from_slice(&self.bytes.to_be_bytes());
        b[44..46].copy_from_slice(&self.samples.to_be_bytes());
        b[46] = self.channels;
        b[47] = self.direction;
        b
    }
    pub fn read(input: &mut impl Read, reply: bool) -> Result<Self, Error> {
        let mut b = [0; HEADER];
        input.read_exact(&mut b).map_err(|_| Error::Fatal)?;
        if &b[..4] != (if reply { b"FROR" } else { b"FROP" })
            || b[4] != 1
            || b[6..8] != [0, 0]
            || !(CONFIG..=PLC).contains(&b[5])
            || !(1..=2).contains(&b[46])
            || b[47] > 1
        {
            return Err(Error::InvalidPayload);
        }
        let n = |i| u64::from_be_bytes(b[i..i + 8].try_into().unwrap());
        let h = Self {
            kind: b[5],
            serial: n(8),
            generation: n(16),
            at: n(24),
            sequence: n(32),
            bytes: u32::from_be_bytes(b[40..44].try_into().unwrap()),
            samples: u16::from_be_bytes(b[44..46].try_into().unwrap()),
            channels: b[46],
            direction: b[47],
        };
        if h.serial == 0 {
            return Err(Error::InvalidPayload);
        }
        Ok(h)
    }
    pub fn channels(self) -> AudioChannels {
        if self.channels == 1 {
            AudioChannels::Mono
        } else {
            AudioChannels::Stereo
        }
    }
    pub fn direction(self) -> AudioDirection {
        if self.direction == 0 {
            AudioDirection::Downlink
        } else {
            AudioDirection::Uplink
        }
    }
    pub fn pcm_reply(mut self) -> Self {
        self.bytes = u32::from(self.samples) * u32::from(self.channels) * 2;
        self
    }
    pub fn config_reply(mut self) -> Self {
        self.bytes = 0;
        self
    }
    pub fn matches(self, config: AudioStreamConfig) -> bool {
        self.generation == config.generation().as_raw()
            && self.channels == config.channels().count()
            && self.direction() == config.direction()
            && u32::from(self.samples) == config.expected_samples_per_frame()
    }
}
pub(super) fn config_body(c: AudioStreamConfig, l: CodecLimits) -> Result<[u8; 8], Error> {
    let mut body = [0; 8];
    for (i, n) in [
        c.frame_duration_ms(),
        c.jitter_target_ms(),
        u16::try_from(l.max_packet_bytes()).map_err(|_| Error::BufferOverflow)?,
        u16::try_from(l.max_decoded_samples()).map_err(|_| Error::BufferOverflow)?,
    ]
    .into_iter()
    .enumerate()
    {
        body[i * 2..i * 2 + 2].copy_from_slice(&n.to_be_bytes());
    }
    Ok(body)
}
pub(super) fn read_config(
    header: Header,
    input: &mut impl Read,
) -> Result<(AudioStreamConfig, CodecLimits), Error> {
    if header.kind != CONFIG
        || header.serial != 1
        || header.bytes != 8
        || header.at != 0
        || header.sequence != 0
    {
        return Err(Error::InvalidPayload);
    }
    let mut body = [0; 8];
    input.read_exact(&mut body).map_err(|_| Error::Fatal)?;
    let word = |i| u16::from_be_bytes([body[i], body[i + 1]]);
    let config = AudioStreamConfig::new(
        header.direction(),
        AudioGeneration::from_raw(header.generation),
        header.channels(),
        word(0),
        word(2),
    )?;
    let limits = CodecLimits::new(usize::from(word(4)), u32::from(word(6)))?;
    if !header.matches(config) {
        return Err(Error::InvalidPayload);
    }
    Ok((config, limits))
}
pub(super) fn read_packet(
    h: Header,
    l: CodecLimits,
    input: &mut impl Read,
) -> Result<AudioAccessUnit, Error> {
    let size = usize::try_from(h.bytes).map_err(|_| Error::BufferOverflow)?;
    if size == 0 || size > l.max_packet_bytes() {
        return Err(Error::BufferOverflow);
    }
    let mut bytes = [0; fr_core::audio::MAX_OPUS_PAYLOAD_BYTES];
    input
        .read_exact(&mut bytes[..size])
        .map_err(|_| Error::Fatal)?;
    let p = AudioAccessUnit::new(
        h.direction(),
        AudioGeneration::from_raw(h.generation),
        h.sequence,
        h.at,
        h.samples,
        false,
        &bytes[..size],
    );
    bytes.fill(0);
    p
}
pub(super) fn write(
    output: &mut impl Write,
    h: Header,
    reply: bool,
    data: &[u8],
) -> Result<(), Error> {
    output
        .write_all(&h.encode(reply))
        .and_then(|()| output.write_all(data))
        .map_err(|_| Error::Fatal)
}
struct Sound(Vec<u8>);
impl Drop for Sound {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}
pub(super) fn write_pcm(
    output: &mut impl Write,
    h: Header,
    pcm: &AudioPcmFrame,
) -> Result<(), Error> {
    if pcm.samples_per_channel() != usize::from(h.samples)
        || pcm.channels() != h.channels()
        || pcm.timestamp_samples() != h.at
        || pcm.generation().as_raw() != h.generation
    {
        return Err(Error::InvalidPayload);
    }
    let mut b = Sound(vec![0; pcm.total_samples() * 2]);
    for (s, out) in pcm
        .samples()
        .iter()
        .zip(b.0.as_chunks_mut::<2>().0.iter_mut())
    {
        out.copy_from_slice(&s.to_le_bytes());
    }
    write(output, h.pcm_reply(), true, &b.0)
}
pub(super) fn read_pcm(input: &mut impl Read, request: Header) -> Result<AudioPcmFrame, Error> {
    let h = Header::read(input, true)?;
    if h != request.pcm_reply()
        || usize::from(h.samples) * usize::from(h.channels) > MAX_PCM_BUFFER_SAMPLES
    {
        return Err(Error::InvalidPayload);
    }
    let mut b = Sound(vec![
        0;
        usize::try_from(h.bytes)
            .map_err(|_| Error::BufferOverflow)?
    ]);
    input.read_exact(&mut b.0).map_err(|_| Error::Fatal)?;
    let mut samples = vec![0i16; b.0.len() / 2];
    for (out, v) in samples.iter_mut().zip(b.0.as_chunks::<2>().0.iter()) {
        *out = i16::from_le_bytes([v[0], v[1]]);
    }
    let result = AudioPcmFrame::from_interleaved(
        AudioGeneration::from_raw(h.generation),
        h.channels(),
        h.at,
        &samples,
    );
    samples.fill(0);
    result
}

#[cfg(test)]
mod tests;
