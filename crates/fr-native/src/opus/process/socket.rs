#![forbid(unsafe_code)]
//! Nonblocking codec IPC for a child owned by the containing supervisor.
//!
//! Reuses the exact private Opus worker framing. This adapter owns no process,
//! spawns no thread and never claims to reap a child. Its parent must retain the
//! actual `Child` and independently observe exit. One bounded request/reply is
//! retained, with an original deadline that partial I/O never restarts.
use super::{
    AudioMediaError as Error, AudioStreamConfig, CodecLimits, DECODE, Phase, SETUP,
    protocol::{self, CONFIG, HEADER, Header, PACKET, PLC},
};
use fr_client::audio::playout::decoder::PolledDecoder;
use fr_media::audio::{AudioAccessUnit, AudioPcmFrame, MAX_PCM_BUFFER_SAMPLES};
use std::{
    io::{Cursor, ErrorKind, Read, Write},
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

const REQUEST_BYTES: usize = HEADER + fr_core::audio::MAX_OPUS_PAYLOAD_BYTES;
const REPLY_BYTES: usize = HEADER + MAX_PCM_BUFFER_SAMPLES * 2;
const IO_TURN: usize = 4;

struct Gate {
    socket: UnixStream,
    stopped: AtomicBool,
}
/// Terminal closure of this exact private codec channel. It is not a process
/// exit receipt: only the containing supervisor can confirm its child's reap.
#[derive(Clone)]
pub struct SocketFence(Arc<Gate>);
impl SocketFence {
    pub fn stop(&self) {
        if !self.0.stopped.swap(true, Ordering::AcqRel) {
            let _ = self.0.socket.shutdown(Shutdown::Both);
        }
    }
    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }
}
impl std::fmt::Debug for SocketFence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusSocketFence")
            .field("stopped", &self.is_stopped())
            .finish_non_exhaustive()
    }
}

/// The device worker polls this socket while servicing native graph demand.
/// No blocking read, write, wait, spawn or reaper is hidden behind this owner.
/// The socket must be supplied by the original local process supervisor.
pub struct SocketDecoder {
    fence: SocketFence,
    limits: CodecLimits,
    phase: Phase,
    config: Option<AudioStreamConfig>,
    serial: u64,
    next: Option<(u64, u64)>,
    request: Option<Header>,
    until: Option<Instant>,
    outgoing: [u8; REQUEST_BYTES],
    outgoing_len: usize,
    written: usize,
    incoming: Box<[u8]>,
    incoming_len: usize,
    received: usize,
}
impl std::fmt::Debug for SocketDecoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SocketOpusDecoder")
            .field("stopped", &self.fence.is_stopped())
            .finish_non_exhaustive()
    }
}
impl SocketDecoder {
    pub fn new(socket: UnixStream, limits: CodecLimits) -> Result<Self, Error> {
        socket.set_nonblocking(true).map_err(|_| Error::Fatal)?;
        let mut incoming = Vec::new();
        incoming
            .try_reserve_exact(REPLY_BYTES)
            .map_err(|_| Error::BufferOverflow)?;
        incoming.resize(REPLY_BYTES, 0);
        Ok(Self {
            fence: SocketFence(Arc::new(Gate {
                socket,
                stopped: AtomicBool::new(false),
            })),
            limits,
            phase: Phase::New,
            config: None,
            serial: 0,
            next: None,
            request: None,
            until: None,
            outgoing: [0; REQUEST_BYTES],
            outgoing_len: 0,
            written: 0,
            incoming: incoming.into_boxed_slice(),
            incoming_len: 0,
            received: 0,
        })
    }
    pub fn fence(&self) -> SocketFence {
        self.fence.clone()
    }
    fn erase(&mut self) {
        self.outgoing.fill(0);
        self.incoming.fill(0);
        self.outgoing_len = 0;
        self.incoming_len = 0;
        self.written = 0;
        self.received = 0;
        self.request = None;
        self.until = None;
    }
    fn fail(&mut self, error: Error) -> Error {
        self.fence.stop();
        self.phase = Phase::Stopped;
        self.erase();
        error
    }
    fn live(&mut self) -> Result<(), Error> {
        if self.phase == Phase::Stopped
            || self.fence.is_stopped()
            || self.until.is_some_and(|until| Instant::now() >= until)
        {
            return Err(self.fail(Error::Fatal));
        }
        Ok(())
    }
    fn queue(&mut self, header: Header, body: &[u8], until: Instant) -> Result<(), Error> {
        self.live()?;
        if self.request.is_some() {
            return Err(Error::Backpressure);
        }
        let length = HEADER
            .checked_add(body.len())
            .ok_or(Error::BufferOverflow)?;
        let reply_bytes = if header.kind == CONFIG {
            0
        } else {
            usize::from(header.samples) * usize::from(header.channels) * 2
        };
        if length > self.outgoing.len()
            || HEADER + reply_bytes > self.incoming.len()
            || usize::try_from(header.bytes).ok() != Some(body.len())
        {
            return Err(Error::BufferOverflow);
        }
        self.outgoing[..HEADER].copy_from_slice(&header.encode(false));
        self.outgoing[HEADER..length].copy_from_slice(body);
        self.outgoing_len = length;
        self.incoming_len = HEADER + reply_bytes;
        self.request = Some(header);
        self.until = Some(until);
        self.serial = header.serial;
        Ok(())
    }
    /// At most four nonblocking system calls per turn. Received headers must
    /// match the original request before the already bounded body is read.
    fn pump(&mut self) -> Result<bool, Error> {
        self.live()?;
        let request = self.request.ok_or(Error::NotConfigured)?;
        for _ in 0..IO_TURN {
            self.live()?;
            let mut socket = &self.fence.0.socket;
            let result = if self.written < self.outgoing_len {
                socket
                    .write(&self.outgoing[self.written..self.outgoing_len])
                    .inspect(|count| {
                        self.written += *count;
                    })
            } else if self.received < self.incoming_len {
                let end = if self.received < HEADER {
                    HEADER
                } else {
                    self.incoming_len
                };
                socket
                    .read(&mut self.incoming[self.received..end])
                    .inspect(|count| {
                        self.received += *count;
                    })
            } else {
                return Ok(true);
            };
            match result {
                Ok(0) => return Err(self.fail(Error::Fatal)),
                Ok(_) => {}
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
                {
                    break;
                }
                Err(_) => return Err(self.fail(Error::Fatal)),
            }
            self.live()?;
            if self.received == HEADER {
                let expected = if request.kind == CONFIG {
                    request.config_reply()
                } else {
                    request.pcm_reply()
                };
                let actual = Header::read(&mut Cursor::new(&self.incoming[..HEADER]), true)
                    .map_err(|error| self.fail(error))?;
                if actual != expected {
                    return Err(self.fail(Error::InvalidPayload));
                }
            }
            if self.received == self.incoming_len {
                return Ok(true);
            }
        }
        Ok(false)
    }
    fn idle(&mut self) -> Result<(), Error> {
        self.live()?;
        let mut byte = [0];
        match (&self.fence.0.socket).read(&mut byte) {
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                Ok(())
            }
            Ok(0) | Err(_) => Err(self.fail(Error::Fatal)),
            Ok(_) => Err(self.fail(Error::InvalidPayload)),
        }
    }
    fn enqueue(&mut self, mut header: Header, body: &[u8]) -> Result<(), Error> {
        if self.phase != Phase::Idle {
            return Err(if self.phase == Phase::Stopped {
                Error::Fatal
            } else {
                Error::Backpressure
            });
        }
        self.idle()?;
        header.serial = self.serial.checked_add(1).ok_or(Error::BufferOverflow)?;
        self.queue(header, body, Instant::now() + DECODE)?;
        self.phase = Phase::Decoding;
        Ok(())
    }
}
impl PolledDecoder for SocketDecoder {
    fn configure(&mut self, config: AudioStreamConfig) -> Result<(), Error> {
        self.live()?;
        if self.phase != Phase::New {
            return Err(Error::InvalidPayload);
        }
        if config.expected_samples_per_frame() > self.limits.max_decoded_samples() {
            return Err(Error::BufferOverflow);
        }
        let mut header = Header::new(config, 1, CONFIG)?;
        let body = protocol::config_body(config, self.limits)?;
        header.bytes = u32::try_from(body.len()).map_err(|_| Error::BufferOverflow)?;
        self.queue(header, &body, Instant::now() + SETUP)?;
        self.config = Some(config);
        self.phase = Phase::Configuring;
        Ok(())
    }
    fn poll_configured(&mut self) -> Result<bool, Error> {
        self.live()?;
        if matches!(self.phase, Phase::Idle | Phase::Decoding) {
            return Ok(true);
        }
        if self.phase != Phase::Configuring {
            return Err(Error::NotConfigured);
        }
        if !self.pump()? {
            return Ok(false);
        }
        self.erase();
        self.phase = Phase::Idle;
        Ok(true)
    }
    fn submit_packet(&mut self, packet: &AudioAccessUnit) -> Result<(), Error> {
        let config = self.config.ok_or(Error::NotConfigured)?;
        if packet.generation() != config.generation()
            || packet.direction() != config.direction()
            || u32::from(packet.duration_samples()) != config.expected_samples_per_frame()
            || packet.payload().len() > self.limits.max_packet_bytes()
        {
            return Err(Error::InvalidPayload);
        }
        let mut header = Header::new(config, 1, PACKET)?;
        header.sequence = packet.sequence();
        header.at = packet.timestamp_samples();
        header.bytes = u32::try_from(packet.payload().len()).map_err(|_| Error::BufferOverflow)?;
        self.enqueue(header, packet.payload())
    }
    fn submit_plc(&mut self, samples: u16) -> Result<Option<AudioPcmFrame>, Error> {
        let config = self.config.ok_or(Error::NotConfigured)?;
        if config.expected_samples_per_frame() != u32::from(samples) {
            return Err(Error::InvalidPayload);
        }
        let (sequence, at) = self.next.ok_or(Error::NeedMoreInput)?;
        let mut header = Header::new(config, 1, PLC)?;
        header.sequence = sequence;
        header.at = at;
        header.samples = samples;
        self.enqueue(header, &[])?;
        Ok(None)
    }
    fn poll_pcm(&mut self) -> Result<Option<AudioPcmFrame>, Error> {
        self.live()?;
        if self.phase == Phase::Idle {
            self.idle()?;
            return Ok(None);
        }
        if self.phase != Phase::Decoding {
            return Err(Error::NotConfigured);
        }
        if !self.pump()? {
            return Ok(None);
        }
        let request = self.request.ok_or(Error::NotConfigured)?;
        let next = request
            .sequence
            .checked_add(1)
            .zip(request.at.checked_add(u64::from(request.samples)))
            .ok_or_else(|| self.fail(Error::BufferOverflow))?;
        let pcm = protocol::read_pcm(
            &mut Cursor::new(&self.incoming[..self.incoming_len]),
            request,
        )
        .map_err(|error| self.fail(error))?;
        self.next = Some(next);
        self.erase();
        self.phase = Phase::Idle;
        Ok(Some(pcm))
    }
}
impl Drop for SocketDecoder {
    fn drop(&mut self) {
        self.fence.stop();
        self.erase();
    }
}
