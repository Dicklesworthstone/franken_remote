//! Private audio-capture worker records (plan §15.4). NOT a network protocol.
//!
//! The parent names a locally selected audio server and playback monitor and
//! the Opus profile; the worker records that monitor, encodes Opus and answers
//! each `ReadAudio` pull with already encoded packets. Bodies carry compressed
//! audio, so no `Debug` exposes payload bytes. Every length, count, duration,
//! sequence and timestamp is validated BEFORE the parent allocates packets.
use super::Error;
use crate::audio::AudioAccessUnit;
use core::fmt;
use fr_core::audio::{
    AudioChannels, AudioDirection, AudioGeneration, MAX_OPUS_PAYLOAD_BYTES, OPUS_SAMPLE_RATE,
};

/// Same bound as the playback selection: a short absolute UNIX socket path.
pub const MAX_SERVER_BYTES: usize = 100;
/// `PulseAudio` object-name bound.
pub const MAX_SINK_BYTES: usize = 255;
/// Packets returned by one pull. A slow parent loses old audio in the server's
/// own bounded record queue; it never receives an unbounded catch-up burst.
pub const MAX_BATCH_PACKETS: usize = 8;
const CONFIG_FIXED_BYTES: usize = 20;
const BATCH_HEADER_BYTES: usize = 12;
const PACKET_PREFIX_BYTES: usize = 20;
/// Largest `AudioPackets` body: header plus the maximum packets at the
/// absolute Opus payload bound.
pub const MAX_BATCH_BYTES: usize =
    BATCH_HEADER_BYTES + MAX_BATCH_PACKETS * (PACKET_PREFIX_BYTES + MAX_OPUS_PAYLOAD_BYTES);

/// The monitor source the worker records. Both are locally chosen; a peer can
/// never name a device.
#[derive(Clone, PartialEq, Eq)]
pub enum Monitor {
    /// The server's default sink's monitor, resolved once at connect and
    /// pinned (the stream is never moved to a later default).
    DefaultSink,
    /// The monitor of one explicitly named sink.
    Sink(String),
}
impl fmt::Debug for Monitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Device names are local configuration, not diagnostics.
        f.write_str(match self {
            Self::DefaultSink => "Monitor(default sink)",
            Self::Sink(_) => "Monitor(named sink)",
        })
    }
}

/// Complete, validated capture/encode profile for one audio generation.
#[derive(Clone, PartialEq, Eq)]
pub struct Capture {
    pub generation: AudioGeneration,
    pub channels: AudioChannels,
    /// 10 or 20 ms; the qualified playback profile.
    pub frame_duration_ms: u16,
    pub bitrate: u32,
    /// Opus payload ceiling; the parent sizes it to its datagram routes.
    pub max_packet_bytes: u16,
    /// Absolute path of the local server's UNIX socket.
    pub server: String,
    pub monitor: Monitor,
}
impl fmt::Debug for Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioCapture")
            .field("generation", &self.generation)
            .field("channels", &self.channels)
            .field("frame_duration_ms", &self.frame_duration_ms)
            .field("bitrate", &self.bitrate)
            .field("max_packet_bytes", &self.max_packet_bytes)
            .field("monitor", &self.monitor)
            .finish_non_exhaustive()
    }
}
fn valid_server(server: &str) -> bool {
    server.starts_with('/')
        && (2..=MAX_SERVER_BYTES).contains(&server.len())
        && !server.bytes().any(|b| b <= b' ' || b == 127 || b == b':')
}
fn valid_sink(sink: &str) -> bool {
    // The worker appends ".monitor"; keep the total within the object bound.
    (1..=MAX_SINK_BYTES - 8).contains(&sink.len())
        && sink
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
}
impl Capture {
    /// Decoded samples per channel in one packet.
    pub const fn frame_samples(&self) -> u16 {
        // 10 or 20 ms at 48 kHz: 480 or 960, always within u16.
        #[allow(clippy::cast_possible_truncation)]
        let samples = (OPUS_SAMPLE_RATE / 1000) as u16 * self.frame_duration_ms;
        samples
    }
    pub fn validate(&self) -> Result<(), Error> {
        let fits = u64::from(self.bitrate) * u64::from(self.frame_duration_ms)
            <= u64::from(self.max_packet_bytes) * 8_000;
        if self.generation.as_raw() == 0
            || !matches!(self.frame_duration_ms, 10 | 20)
            || !(6_000..=192_000).contains(&self.bitrate)
            || self.max_packet_bytes == 0
            || usize::from(self.max_packet_bytes) > MAX_OPUS_PAYLOAD_BYTES
            || !fits
            || !valid_server(&self.server)
            || matches!(&self.monitor, Monitor::Sink(s) if !valid_sink(s))
        {
            return Err(Error::Malformed);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let sink = match &self.monitor {
            Monitor::DefaultSink => "",
            Monitor::Sink(s) => s.as_str(),
        };
        let mut b = Vec::with_capacity(CONFIG_FIXED_BYTES + self.server.len() + sink.len());
        b.extend_from_slice(&self.generation.as_raw().to_be_bytes());
        b.push(self.channels.count());
        b.push(u8::try_from(self.frame_duration_ms).map_err(|_| Error::Malformed)?);
        b.extend_from_slice(&self.max_packet_bytes.to_be_bytes());
        b.extend_from_slice(&self.bitrate.to_be_bytes());
        b.push(u8::try_from(self.server.len()).map_err(|_| Error::Malformed)?);
        b.push(u8::try_from(sink.len()).map_err(|_| Error::Malformed)?);
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(self.server.as_bytes());
        b.extend_from_slice(sink.as_bytes());
        Ok(b)
    }
    pub fn decode(b: &[u8]) -> Result<Self, Error> {
        if !accepts_configuration_length(b.len()) || b[18..20] != [0, 0] {
            return Err(Error::Malformed);
        }
        let server_len = usize::from(b[16]);
        let sink_len = usize::from(b[17]);
        if CONFIG_FIXED_BYTES
            .checked_add(server_len)
            .and_then(|n| n.checked_add(sink_len))
            != Some(b.len())
        {
            return Err(Error::Malformed);
        }
        let text = |range: core::ops::Range<usize>| {
            core::str::from_utf8(&b[range])
                .map(str::to_owned)
                .map_err(|_| Error::Malformed)
        };
        let server = text(CONFIG_FIXED_BYTES..CONFIG_FIXED_BYTES + server_len)?;
        let monitor = if sink_len == 0 {
            Monitor::DefaultSink
        } else {
            Monitor::Sink(text(CONFIG_FIXED_BYTES + server_len..b.len())?)
        };
        let capture = Self {
            generation: AudioGeneration::from_raw(u64::from_be_bytes(
                b[..8].try_into().map_err(|_| Error::Malformed)?,
            )),
            channels: AudioChannels::from_u8(b[8]).ok_or(Error::Malformed)?,
            frame_duration_ms: u16::from(b[9]),
            max_packet_bytes: u16::from_be_bytes([b[10], b[11]]),
            bitrate: u32::from_be_bytes(b[12..16].try_into().map_err(|_| Error::Malformed)?),
            server,
            monitor,
        };
        capture.validate()?;
        Ok(capture)
    }
}
pub(super) fn accepts_configuration_length(length: usize) -> bool {
    (CONFIG_FIXED_BYTES + 2..=CONFIG_FIXED_BYTES + MAX_SERVER_BYTES + MAX_SINK_BYTES)
        .contains(&length)
}
pub(super) fn accepts_batch_length(length: usize) -> bool {
    (BATCH_HEADER_BYTES..=MAX_BATCH_BYTES).contains(&length)
}

/// Content-free worker-side accounting reported with every pull.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureCounters {
    /// Explicit capture discontinuities (server holes, dropped partial frames).
    pub gaps: u32,
    /// Pulls that found the bounded record queue full: older audio may have
    /// been discarded by the audio server before this worker read it.
    pub overruns: u32,
}

/// One validated pull result. Packets are consecutive in sequence, strictly
/// increasing in source time and exactly one negotiated frame long.
pub struct Batch {
    pub packets: Vec<AudioAccessUnit>,
    pub counters: CaptureCounters,
}
impl fmt::Debug for Batch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AudioBatch")
            .field("packets", &self.packets.len())
            .field("counters", &self.counters)
            .finish()
    }
}

/// Worker-side encode of already produced packets.
pub fn encode_batch(
    packets: &[AudioAccessUnit],
    counters: CaptureCounters,
    capture: &Capture,
) -> Result<Vec<u8>, Error> {
    if packets.len() > MAX_BATCH_PACKETS {
        return Err(Error::ResourceLimit);
    }
    let mut bytes = BATCH_HEADER_BYTES;
    for p in packets {
        if p.generation() != capture.generation
            || p.direction() != AudioDirection::Downlink
            || p.duration_samples() != capture.frame_samples()
            || p.payload().len() > usize::from(capture.max_packet_bytes)
        {
            return Err(Error::Malformed);
        }
        bytes += PACKET_PREFIX_BYTES + p.payload().len();
    }
    let mut b = Vec::new();
    b.try_reserve_exact(bytes).map_err(|_| Error::Allocation)?;
    b.push(u8::try_from(packets.len()).map_err(|_| Error::ResourceLimit)?);
    b.extend_from_slice(&[0; 3]);
    b.extend_from_slice(&counters.gaps.to_be_bytes());
    b.extend_from_slice(&counters.overruns.to_be_bytes());
    for p in packets {
        b.extend_from_slice(&p.sequence().to_be_bytes());
        b.extend_from_slice(&p.timestamp_samples().to_be_bytes());
        b.extend_from_slice(&p.duration_samples().to_be_bytes());
        b.extend_from_slice(
            &u16::try_from(p.payload().len())
                .map_err(|_| Error::Malformed)?
                .to_be_bytes(),
        );
        b.extend_from_slice(p.payload());
    }
    Ok(b)
}

/// Parent-side decode. The whole body's framing is checked before the packet
/// vector is allocated; each payload is bounded by the configured ceiling.
pub fn decode_batch(body: &[u8], capture: &Capture) -> Result<Batch, Error> {
    if !accepts_batch_length(body.len()) || body[1..4] != [0; 3] {
        return Err(Error::Malformed);
    }
    let count = usize::from(body[0]);
    if count > MAX_BATCH_PACKETS {
        return Err(Error::ResourceLimit);
    }
    let u16_at = |at: usize| u16::from_be_bytes([body[at], body[at + 1]]);
    let u64_at = |at: usize| {
        u64::from_be_bytes(
            body[at..at + 8]
                .try_into()
                .expect("framing was length-checked"),
        )
    };
    // Pass one: framing only, no allocation.
    let mut at = BATCH_HEADER_BYTES;
    for _ in 0..count {
        let prefix_end = at
            .checked_add(PACKET_PREFIX_BYTES)
            .filter(|&n| n <= body.len())
            .ok_or(Error::Malformed)?;
        let len = usize::from(u16_at(at + 18));
        if len == 0 || len > usize::from(capture.max_packet_bytes) {
            return Err(Error::ResourceLimit);
        }
        at = prefix_end
            .checked_add(len)
            .filter(|&n| n <= body.len())
            .ok_or(Error::Malformed)?;
    }
    if at != body.len() {
        return Err(Error::Malformed);
    }
    let counters = CaptureCounters {
        gaps: u32::from_be_bytes(body[4..8].try_into().map_err(|_| Error::Malformed)?),
        overruns: u32::from_be_bytes(body[8..12].try_into().map_err(|_| Error::Malformed)?),
    };
    let mut packets = Vec::new();
    packets
        .try_reserve_exact(count)
        .map_err(|_| Error::Allocation)?;
    let mut at = BATCH_HEADER_BYTES;
    let mut last: Option<(u64, u64)> = None;
    let frame = u64::from(capture.frame_samples());
    for _ in 0..count {
        let sequence = u64_at(at);
        let timestamp = u64_at(at + 8);
        let duration = u16_at(at + 16);
        let len = usize::from(u16_at(at + 18));
        let payload = &body[at + PACKET_PREFIX_BYTES..at + PACKET_PREFIX_BYTES + len];
        if duration != capture.frame_samples()
            || timestamp.checked_add(frame).is_none()
            || last.is_some_and(|(s, t)| {
                s.checked_add(1) != Some(sequence)
                    || t.checked_add(frame).is_none_or(|e| timestamp < e)
            })
        {
            return Err(Error::Malformed);
        }
        packets.push(
            AudioAccessUnit::new(
                AudioDirection::Downlink,
                capture.generation,
                sequence,
                timestamp,
                duration,
                false,
                payload,
            )
            .map_err(|_| Error::Malformed)?,
        );
        last = Some((sequence, timestamp));
        at += PACKET_PREFIX_BYTES + len;
    }
    Ok(Batch { packets, counters })
}

#[cfg(test)]
mod tests;
