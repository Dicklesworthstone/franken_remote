#![forbid(unsafe_code)]
//! Private host-microphone worker IPC, never a remote protocol or a grant.
//! Parent-created sockets are the capability. Absolute same-boot deadlines
//! survive partial reads and worker custody; stopping uses a separate EOF gate.
use fr_core::audio::{AudioDirection, AudioStreamConfig, MAX_OPUS_PAYLOAD_BYTES};
use fr_wire::audio::{self, AudioConfiguration};
use std::io::{Read, Write};

pub const HEADER_BYTES: usize = 64;
pub const MAX_BODY_BYTES: usize = audio::AUDIO_PACKET_OVERHEAD + MAX_OPUS_PAYLOAD_BYTES;
pub const MAX_SERVER_BYTES: usize = 100;
pub const SNAPSHOT_BYTES: usize = 40;
pub const MAX_RECORD_NS: u64 = 100_000_000;
pub const SETUP_NS: u64 = 2_000_000_000;
pub const MAX_AUTHORITY_NS: u64 = 3_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Configure = 1,
    Commit = 2,
    Packet = 3,
    Health = 4,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Status {
    Ok = 0,
    Denied = 1,
    Expired = 2,
    Protocol = 3,
    Native = 4,
    Stopped = 5,
    NotReady = 6,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Malformed,
    Limit,
    Io,
    Clock,
    Expired,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "microphone-worker: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub status: Status,
    pub serial: u64,
    pub generation: u64,
    pub authorization_ns: u64,
    pub arrived_ns: u64,
    pub expires_ns: u64,
    pub binding: u32,
    pub bytes: u32,
}
impl Header {
    pub fn validate(self, reply: bool) -> Result<(), Error> {
        if self.serial == 0
            || self.generation == 0
            || self.binding == 0
            || self.arrived_ns == 0
            || self.expires_ns <= self.arrived_ns
            || self.expires_ns > self.authorization_ns
            || usize::try_from(self.bytes).map_or(true, |bytes| bytes > MAX_BODY_BYTES)
            || (!reply && self.status != Status::Ok)
            || (!reply && matches!(self.kind, Kind::Commit | Kind::Health) && self.bytes != 0)
            || (self.kind == Kind::Packet && self.expires_ns - self.arrived_ns > MAX_RECORD_NS)
        {
            return Err(Error::Malformed);
        }
        Ok(())
    }
    pub fn live(self, now_ns: u64) -> Result<(), Error> {
        if self.arrived_ns > now_ns
            || self.authorization_ns.saturating_sub(now_ns) > MAX_AUTHORITY_NS
        {
            return Err(Error::Clock);
        }
        if now_ns >= self.expires_ns || now_ns >= self.authorization_ns {
            return Err(Error::Expired);
        }
        Ok(())
    }
    pub fn encode(self, reply: bool) -> Result<[u8; HEADER_BYTES], Error> {
        self.validate(reply)?;
        let mut bytes = [0; HEADER_BYTES];
        bytes[..4].copy_from_slice(if reply { b"FMPR" } else { b"FMPQ" });
        bytes[4] = 1;
        bytes[5] = self.kind as u8;
        bytes[6..8].copy_from_slice(&(self.status as u16).to_be_bytes());
        for (index, value) in [
            self.serial,
            self.generation,
            self.authorization_ns,
            self.arrived_ns,
            self.expires_ns,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[8 + index * 8..16 + index * 8].copy_from_slice(&value.to_be_bytes());
        }
        bytes[48..52].copy_from_slice(&self.binding.to_be_bytes());
        bytes[52..56].copy_from_slice(&self.bytes.to_be_bytes());
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8; HEADER_BYTES], reply: bool) -> Result<Self, Error> {
        if &bytes[..4] != (if reply { b"FMPR" } else { b"FMPQ" })
            || bytes[4] != 1
            || bytes[56..] != [0; 8]
        {
            return Err(Error::Malformed);
        }
        let kind = match bytes[5] {
            1 => Kind::Configure,
            2 => Kind::Commit,
            3 => Kind::Packet,
            4 => Kind::Health,
            _ => return Err(Error::Malformed),
        };
        let status = match u16::from_be_bytes([bytes[6], bytes[7]]) {
            0 => Status::Ok,
            1 => Status::Denied,
            2 => Status::Expired,
            3 => Status::Protocol,
            4 => Status::Native,
            5 => Status::Stopped,
            6 => Status::NotReady,
            _ => return Err(Error::Malformed),
        };
        let word = |at| u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());
        let header = Self {
            kind,
            status,
            serial: word(8),
            generation: word(16),
            authorization_ns: word(24),
            arrived_ns: word(32),
            expires_ns: word(40),
            binding: u32::from_be_bytes(bytes[48..52].try_into().unwrap()),
            bytes: u32::from_be_bytes(bytes[52..56].try_into().unwrap()),
        };
        header.validate(reply)?;
        Ok(header)
    }
    pub fn reply(self, status: Status, bytes: usize) -> Result<Self, Error> {
        let reply = Self {
            status,
            bytes: u32::try_from(bytes).map_err(|_| Error::Limit)?,
            ..self
        };
        reply.validate(true)?;
        Ok(reply)
    }
    pub fn matches(self, request: Self) -> bool {
        self.kind == request.kind
            && self.serial == request.serial
            && self.generation == request.generation
            && self.binding == request.binding
            && self.authorization_ns == request.authorization_ns
            && self.arrived_ns == request.arrived_ns
            && self.expires_ns == request.expires_ns
    }
}

pub struct Record {
    pub header: Header,
    pub body: Vec<u8>,
}
impl std::fmt::Debug for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneWorkerRecord")
            .field("header", &self.header)
            .field("bytes", &self.body.len())
            .finish()
    }
}
impl Drop for Record {
    fn drop(&mut self) {
        self.body.fill(0);
    }
}
impl Record {
    pub fn write(&self, output: &mut impl Write, reply: bool) -> Result<(), Error> {
        if usize::try_from(self.header.bytes).ok() != Some(self.body.len()) {
            return Err(Error::Malformed);
        }
        output
            .write_all(&self.header.encode(reply)?)
            .and_then(|()| output.write_all(&self.body))
            .map_err(|_| Error::Io)
    }
    pub fn read(input: &mut impl Read, reply: bool) -> Result<Self, Error> {
        let mut encoded = [0; HEADER_BYTES];
        input.read_exact(&mut encoded).map_err(|_| Error::Io)?;
        let header = Header::decode(&encoded, reply)?;
        let len = usize::try_from(header.bytes).map_err(|_| Error::Limit)?;
        let mut body = Vec::new();
        body.try_reserve_exact(len).map_err(|_| Error::Limit)?;
        body.resize(len, 0);
        let mut record = Self { header, body };
        input.read_exact(&mut record.body).map_err(|_| Error::Io)?;
        Ok(record)
    }
}

/// Explicit local server/profile; this never comes from a remote device path.
#[derive(Clone)]
pub struct Configuration {
    pub server: String,
    pub offer: AudioConfiguration,
    /// Complete FRD0 packet ceiling of the original attachment.
    pub packet_maximum: usize,
}
impl std::fmt::Debug for Configuration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneWorkerConfiguration")
            .field("offer", &self.offer)
            .field("packet_maximum", &self.packet_maximum)
            .finish_non_exhaustive()
    }
}
impl Configuration {
    pub fn validate(&self) -> Result<AudioStreamConfig, Error> {
        self.offer.validate().map_err(|_| Error::Malformed)?;
        if self.offer.direction != AudioDirection::Uplink
            || self.offer.generation.as_raw() == 0
            || !matches!(self.offer.frame_duration_ms, 10 | 20)
            || self.packet_maximum > MAX_BODY_BYTES
            || audio::packet_record_bytes(
                usize::try_from(self.offer.max_packet_bytes).map_err(|_| Error::Limit)?,
            )
            .is_none_or(|bytes| bytes > self.packet_maximum)
            || !self.server.starts_with('/')
            || !(2..=MAX_SERVER_BYTES).contains(&self.server.len())
            || self.server.bytes().any(|byte| byte <= b' ' || byte == 127)
            || self
                .server
                .split('/')
                .skip(1)
                .any(|part| matches!(part, "" | "." | ".."))
        {
            return Err(Error::Malformed);
        }
        let config = AudioStreamConfig::new(
            self.offer.direction,
            self.offer.generation,
            self.offer.channels,
            self.offer.frame_duration_ms,
            self.offer.jitter_target_ms,
        )
        .map_err(|_| Error::Malformed)?;
        if config.expected_samples_per_frame() > self.offer.max_decoded_samples {
            return Err(Error::Limit);
        }
        Ok(config)
    }
    pub fn encode(&self, binding: u32) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let mut bytes = vec![0; 4 + audio::AUDIO_CONFIGURATION_RECORD_BYTES];
        bytes[..2].copy_from_slice(
            &u16::try_from(self.packet_maximum)
                .map_err(|_| Error::Limit)?
                .to_be_bytes(),
        );
        bytes[2] = u8::try_from(self.server.len()).map_err(|_| Error::Limit)?;
        audio::encode_configuration(&self.offer, binding, &mut bytes[4..])
            .map_err(|_| Error::Malformed)?;
        bytes.extend_from_slice(self.server.as_bytes());
        Ok(bytes)
    }
    pub fn decode(header: Header, bytes: &[u8]) -> Result<Self, Error> {
        let prefix = 4 + audio::AUDIO_CONFIGURATION_RECORD_BYTES;
        if header.kind != Kind::Configure
            || header.serial != 1
            || bytes.len() < prefix
            || bytes[3] != 0
            || prefix + usize::from(bytes[2]) != bytes.len()
        {
            return Err(Error::Malformed);
        }
        let offer = audio::decode_configuration(&bytes[4..prefix], header.binding)
            .map_err(|_| Error::Malformed)?;
        if offer.generation.as_raw() != header.generation {
            return Err(Error::Malformed);
        }
        let configuration = Self {
            server: std::str::from_utf8(&bytes[prefix..])
                .map_err(|_| Error::Malformed)?
                .into(),
            offer,
            packet_maximum: usize::from(u16::from_be_bytes([bytes[0], bytes[1]])),
        };
        configuration.validate()?;
        Ok(configuration)
    }
}

/// Native custody counters and exact source identity, never proof an app heard sound.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub node_id: u32,
    pub failure_code: u32,
    pub submitted_frames: u64,
    pub silent_frames: u64,
    pub expired_frames: u64,
    pub queued_frames: u32,
    pub queued_packets: u16,
    pub admitted_packet: bool,
}
impl Snapshot {
    pub fn encode(self) -> [u8; SNAPSHOT_BYTES] {
        let mut bytes = [0; SNAPSHOT_BYTES];
        bytes[..4].copy_from_slice(&self.node_id.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.failure_code.to_be_bytes());
        for (index, value) in [
            self.submitted_frames,
            self.silent_frames,
            self.expired_frames,
        ]
        .into_iter()
        .enumerate()
        {
            bytes[8 + index * 8..16 + index * 8].copy_from_slice(&value.to_be_bytes());
        }
        bytes[32..36].copy_from_slice(&self.queued_frames.to_be_bytes());
        bytes[36..38].copy_from_slice(&self.queued_packets.to_be_bytes());
        bytes[38] = u8::from(self.admitted_packet);
        bytes
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() != SNAPSHOT_BYTES || bytes[38] > 1 || bytes[39] != 0 {
            return Err(Error::Malformed);
        }
        let word = |at| u64::from_be_bytes(bytes[at..at + 8].try_into().unwrap());
        let snapshot = Self {
            node_id: u32::from_be_bytes(bytes[..4].try_into().unwrap()),
            failure_code: u32::from_be_bytes(bytes[4..8].try_into().unwrap()),
            submitted_frames: word(8),
            silent_frames: word(16),
            expired_frames: word(24),
            queued_frames: u32::from_be_bytes(bytes[32..36].try_into().unwrap()),
            queued_packets: u16::from_be_bytes([bytes[36], bytes[37]]),
            admitted_packet: bytes[38] != 0,
        };
        if snapshot.queued_packets > 16 || snapshot.queued_frames > 4 * 961 {
            return Err(Error::Limit);
        }
        Ok(snapshot)
    }
}

/// One bounded nonblocking record accumulator. Each turn performs at most four
/// reads. It never scans for a new magic after a malformed or partial record.
/// The containing owner enforces the original exchange/authority deadline even
/// before a complete header is available.
pub struct Incoming {
    bytes: [u8; HEADER_BYTES + MAX_BODY_BYTES],
    read: usize,
    header: Option<Header>,
}
impl Default for Incoming {
    fn default() -> Self {
        Self {
            bytes: [0; HEADER_BYTES + MAX_BODY_BYTES],
            read: 0,
            header: None,
        }
    }
}
impl Drop for Incoming {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}
impl Incoming {
    pub fn is_partial(&self) -> bool {
        self.read != 0
    }
    pub fn header(&self) -> Option<Header> {
        self.header
    }
    pub fn poll(&mut self, input: &mut impl Read, reply: bool) -> Result<Option<Record>, Error> {
        for _ in 0..4 {
            let end = self
                .header
                .map_or(HEADER_BYTES, |header| HEADER_BYTES + header.bytes as usize);
            if self.read == end {
                if let Some(header) = self.header {
                    let mut body = Vec::new();
                    body.try_reserve_exact(header.bytes as usize)
                        .map_err(|_| Error::Limit)?;
                    body.extend_from_slice(&self.bytes[HEADER_BYTES..end]);
                    self.bytes[..end].fill(0);
                    self.read = 0;
                    self.header = None;
                    return Ok(Some(Record { header, body }));
                }
                self.header = Some(Header::decode(
                    self.bytes[..HEADER_BYTES].try_into().unwrap(),
                    reply,
                )?);
                continue;
            }
            match input.read(&mut self.bytes[self.read..end]) {
                Ok(0) => return Err(Error::Io),
                Ok(count) => self.read += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Error::Io),
            }
        }
        Ok(None)
    }
}

/// One bounded outgoing record. Completion proves only socket custody. This
/// owner retains the original header so partial writes cannot replace its cap.
pub struct Outgoing {
    bytes: [u8; HEADER_BYTES + MAX_BODY_BYTES],
    written: usize,
    length: usize,
    header: Header,
}
impl Outgoing {
    pub fn new(record: &Record, reply: bool) -> Result<Self, Error> {
        let header = record.header.encode(reply)?;
        if record.body.len() != record.header.bytes as usize {
            return Err(Error::Malformed);
        }
        let mut pending = Self {
            bytes: [0; HEADER_BYTES + MAX_BODY_BYTES],
            written: 0,
            length: HEADER_BYTES + record.body.len(),
            header: record.header,
        };
        pending.bytes[..HEADER_BYTES].copy_from_slice(&header);
        pending.bytes[HEADER_BYTES..pending.length].copy_from_slice(&record.body);
        Ok(pending)
    }
    pub fn header(&self) -> Header {
        self.header
    }
    pub fn poll(&mut self, output: &mut impl Write) -> Result<bool, Error> {
        for _ in 0..4 {
            if self.written == self.length {
                return Ok(true);
            }
            match output.write(&self.bytes[self.written..self.length]) {
                Ok(0) => return Err(Error::Io),
                Ok(count) => self.written += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return Err(Error::Io),
            }
        }
        Ok(self.written == self.length)
    }
}
impl Drop for Outgoing {
    fn drop(&mut self) {
        self.bytes.fill(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind};

    fn header(kind: Kind, bytes: u32) -> Header {
        Header {
            kind,
            status: Status::Ok,
            serial: 1,
            generation: 7,
            authorization_ns: 500_000_000,
            arrived_ns: 10_000_000,
            expires_ns: 100_000_000,
            binding: 73,
            bytes,
        }
    }
    #[test]
    fn private_header_rejects_bounds_reserved_and_cross_exchange_replies() {
        let original = header(Kind::Packet, 3);
        let mut bytes = original.encode(false).unwrap();
        assert_eq!(Header::decode(&bytes, false).unwrap(), original);
        bytes[63] = 1;
        assert_eq!(Header::decode(&bytes, false), Err(Error::Malformed));
        bytes[63] = 0;
        bytes[52..56].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(Header::decode(&bytes, false), Err(Error::Malformed));
        assert!(original.reply(Status::Ok, 40).unwrap().matches(original));
        let mut replacement = original;
        replacement.serial += 1;
        assert!(!replacement.reply(Status::Ok, 40).unwrap().matches(original));
        replacement = original;
        replacement.expires_ns = original.arrived_ns + MAX_RECORD_NS + 1;
        assert_eq!(replacement.encode(false), Err(Error::Malformed));
        assert_eq!(original.live(original.expires_ns), Err(Error::Expired));
        assert_eq!(original.live(original.arrived_ns - 1), Err(Error::Clock));
    }
    struct Partial {
        input: Cursor<Vec<u8>>,
        remaining: usize,
    }
    impl Read for Partial {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(ErrorKind::WouldBlock.into());
            }
            let length = bytes.len().min(self.remaining);
            let count = self.input.read(&mut bytes[..length])?;
            self.remaining -= count;
            Ok(count)
        }
    }
    #[test]
    fn partial_private_record_retains_original_header_and_exact_body_bound() {
        let record = Record {
            header: header(Kind::Packet, 3),
            body: vec![1, 2, 3],
        };
        let mut encoded = Vec::new();
        record.write(&mut encoded, false).unwrap();
        let mut input = Partial {
            input: Cursor::new(encoded),
            remaining: HEADER_BYTES + 1,
        };
        let mut incoming = Incoming::default();
        assert!(incoming.poll(&mut input, false).unwrap().is_none());
        assert!(incoming.is_partial());
        assert_eq!(incoming.header(), Some(record.header));
        assert_eq!(
            incoming.header().unwrap().live(100_000_000),
            Err(Error::Expired)
        );
        input.remaining = 2;
        let completed = incoming.poll(&mut input, false).unwrap().unwrap();
        assert_eq!(completed.header, record.header);
        assert_eq!(completed.body, record.body);
        assert!(!incoming.is_partial());
    }
    #[test]
    fn configuration_requires_explicit_uplink_profile_and_complete_packet_ceiling() {
        use fr_core::audio::{AudioChannels, AudioGeneration};
        let mut configuration = Configuration {
            server: "/private/pipewire-0".into(),
            packet_maximum: audio::AUDIO_PACKET_OVERHEAD + 1000,
            offer: AudioConfiguration {
                direction: AudioDirection::Uplink,
                generation: AudioGeneration::from_raw(7),
                channels: AudioChannels::Mono,
                sample_rate: 48_000,
                frame_duration_ms: 10,
                max_packet_bytes: 1000,
                max_decoded_samples: 480,
                jitter_target_ms: 20,
            },
        };
        let encoded = configuration.encode(73).unwrap();
        let parsed = Configuration::decode(
            header(Kind::Configure, u32::try_from(encoded.len()).unwrap()),
            &encoded,
        )
        .unwrap();
        assert_eq!(parsed.server, configuration.server);
        configuration.packet_maximum -= 1;
        assert_eq!(configuration.validate(), Err(Error::Malformed));
        configuration.packet_maximum += 1;
        configuration.offer.direction = AudioDirection::Downlink;
        assert_eq!(configuration.validate(), Err(Error::Malformed));
        configuration.offer.direction = AudioDirection::Uplink;
        configuration.server = "/private/../pipewire-0".into();
        assert_eq!(configuration.validate(), Err(Error::Malformed));
    }
}
