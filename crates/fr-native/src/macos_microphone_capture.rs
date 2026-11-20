//! Actual `CoreAudio` microphone + Opus inside the original private child. Native
//! callbacks retain only their C owner; Rust preserves original sample times
//! and pull deadlines through encoding and private IPC.
use crate::{
    audio_process::Error,
    clock,
    opus::{CodecLimits, Encoder},
};
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_media::{
    audio::{AudioAccessUnit, AudioEncoder, AudioPcmFrame},
    worker::audio::CaptureCounters,
};
use std::{
    ffi::{CString, c_int, c_void},
    ptr::NonNull,
    time::{Duration, Instant},
};

unsafe extern "C" {
    fn fr_macos_mic_create(
        uid: *const i8,
        guard_fd: c_int,
        authorization_ns: u64,
        setup_until_ns: u64,
        out: *mut *mut c_void,
    ) -> c_int;
    fn fr_macos_mic_renew(owner: *mut c_void, original: u64, next: u64) -> c_int;
    fn fr_macos_mic_read(
        owner: *mut c_void,
        samples: *mut i16,
        capacity: usize,
        count: *mut u32,
        sequence: *mut u64,
        position: *mut u64,
        captured: *mut u64,
    ) -> c_int;
    fn fr_macos_mic_destroy(owner: *mut c_void) -> c_int;
    fn fr_mic_bind_parent(parent: u32) -> c_int;
    fn fr_mic_authorize(request_until_ns: u64, wait_until_ns: u64) -> c_int;
    fn fr_macos_mic_default_uid(bytes: *mut u8, capacity: usize, count: *mut usize) -> c_int;
    #[cfg(feature = "macos-microphone")]
    fn fr_macos_mic_sleep_offset(lower: *mut u64, upper: *mut u64) -> c_int;
}

/// Immutable sleep-offset evidence used only as an additional negative veto.
/// Per-packet custody separately keeps its exact raw monotonic deadline.
#[cfg(feature = "macos-microphone")]
pub(crate) fn sleep_offset() -> Option<(u64, u64)> {
    let (mut lower, mut upper) = (0, 0);
    // SAFETY: the native sampler touches only these writable scalars, retains
    // no pointer and makes no device, permission, thread or AppKit call.
    (unsafe { fr_macos_mic_sleep_offset(&raw mut lower, &raw mut upper) } == 0 && lower <= upper)
        .then_some((lower, upper))
}

/// Resolve an explicitly chosen local default once. This reads device metadata
/// only; it neither prompts for microphone access nor opens a recording queue.
pub fn default_uid() -> Result<String, Error> {
    let mut bytes = [0; 255];
    let mut count = 0;
    // SAFETY: C writes at most the fixed capacity and returns one whole UTF-8
    // UID with a checked count. It retains no pointer or selected device owner.
    if !status(unsafe {
        fr_macos_mic_default_uid(bytes.as_mut_ptr(), bytes.len(), &raw mut count)
    })? || count == 0
        || count > bytes.len()
    {
        return Err(Error::Selection);
    }
    let uid = std::str::from_utf8(&bytes[..count]).map_err(|_| Error::Selection)?;
    if !crate::microphone_selection::Selection::valid_mac_uid(uid) {
        return Err(Error::Selection);
    }
    Ok(uid.to_owned())
}

pub(crate) fn status(code: c_int) -> Result<bool, Error> {
    match code {
        0 => Ok(true),
        1 => Ok(false),
        -1 => Err(Error::Configuration),
        -2 => Err(Error::Selection),
        -3 => Err(Error::Unavailable),
        -4 => Err(Error::Denied),
        -5 => Err(Error::Expired),
        -6 => Err(Error::Clock),
        -8 => Err(Error::CaptureDiscontinuity),
        -9 => Err(Error::Allocation),
        _ => Err(Error::Native),
    }
}

pub(crate) fn bind_parent(parent: u32) -> Result<(), Error> {
    // SAFETY: the private child calls this once before any capture initialization;
    // C validates inherited sockets and installs a kernel parent-death watcher.
    status(unsafe { fr_mic_bind_parent(parent) })?
        .then_some(())
        .ok_or(Error::Spawn)
}

/// Explicit permission-only child role; no microphone object is allocated.
pub fn authorize(parent: u32) -> Result<(), Error> {
    use crate::audio_process::permission_protocol::{BYTES, Request};
    use std::io::Read;
    bind_parent(parent)?;
    let mut bytes = [0; BYTES];
    std::io::stdin()
        .lock()
        .read_exact(&mut bytes)
        .map_err(|_| Error::Protocol)?;
    let request = Request::decode(bytes).ok_or(Error::Protocol)?;
    if !clock::monotonic_ns().is_some_and(|now| request.may_launch(now)) {
        return Err(Error::Expired);
    }
    // SAFETY: the synchronous bounded native permission request retains its own
    // callback storage, verifies fd2 and never calls into Rust or opens a device.
    status(unsafe { fr_mic_authorize(request.launch_until_ns, request.wait_until_ns) })?
        .then_some(())
        .ok_or(Error::Denied)
}

pub struct Batch {
    packets: Vec<AudioAccessUnit>,
    expires: Instant,
    expires_ns: u64,
}
impl Batch {
    pub const fn expires(&self) -> Instant {
        self.expires
    }
    pub const fn expires_ns(&self) -> u64 {
        self.expires_ns
    }
    pub const fn counters(&self) -> CaptureCounters {
        CaptureCounters {
            gaps: 0,
            overruns: 0,
        }
    }
    pub fn consume(
        self,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Vec<AudioAccessUnit>, Error> {
        if !permitted() {
            return Err(Error::Denied);
        }
        if Instant::now() >= self.expires
            || clock::monotonic_ns().is_none_or(|now| now >= self.expires_ns)
        {
            return Err(Error::Expired);
        }
        Ok(self.packets)
    }
}

pub struct MicrophoneSource {
    raw: Option<NonNull<c_void>>,
    encoder: Encoder,
    configuration: AudioStreamConfig,
    next: Option<(u64, u64)>,
}
impl MicrophoneSource {
    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        uid: &str,
        fd: c_int,
        authorization: u64,
        setup: u64,
        configuration: AudioStreamConfig,
        bitrate: u32,
        max_packet: u16,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Self, Error> {
        if !crate::microphone_selection::Selection::valid_mac_uid(uid)
            || configuration.direction() != AudioDirection::Uplink
            || configuration.channels().count() != 1
            || configuration.frame_duration_ms() != 10
        {
            return Err(Error::Configuration);
        }
        let uid = CString::new(uid).map_err(|_| Error::Selection)?;
        let limits =
            CodecLimits::new(usize::from(max_packet), 480).map_err(|_| Error::Configuration)?;
        let mut encoder = Encoder::with_limits(limits, bitrate, 5).map_err(|_| Error::Native)?;
        encoder
            .configure(configuration)
            .map_err(|_| Error::Native)?;
        if !permitted() {
            return Err(Error::Denied);
        }
        let mut raw = std::ptr::null_mut();
        // SAFETY: UID is a live NUL-terminated string, output is writable and C
        // duplicates the original nonblocking guard fd. All ownership returns here.
        let result = status(unsafe {
            fr_macos_mic_create(uid.as_ptr(), fd, authorization, setup, &raw mut raw)
        });
        let mut source = Self {
            raw: NonNull::new(raw),
            encoder,
            configuration,
            next: None,
        };
        if !matches!(result, Ok(true)) || source.raw.is_none() || !permitted() {
            source.close();
            return Err(result.err().unwrap_or(Error::Denied));
        }
        Ok(source)
    }
    pub fn poll_ready(&mut self, mut permitted: impl FnMut() -> bool) -> Result<bool, Error> {
        if self.raw.is_none() || !permitted() {
            return Err(Error::Denied);
        }
        Ok(true)
    }
    pub fn renew(&mut self, original: u64, next: u64) -> Result<(), Error> {
        let raw = self.raw.ok_or(Error::Stopped)?;
        // SAFETY: the unique native owner remains alive. C checks the installed
        // original deadline atomically before accepting this in-order renewal.
        status(unsafe { fr_macos_mic_renew(raw.as_ptr(), original, next) })?
            .then_some(())
            .ok_or(Error::Expired)
    }
    pub fn pull(
        &mut self,
        expires: Instant,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Batch, Error> {
        let raw = self.raw.ok_or(Error::Stopped)?;
        if expires
            > Instant::now()
                .checked_add(Duration::from_millis(100))
                .ok_or(Error::Clock)?
        {
            return Err(Error::Configuration);
        }
        let mut until = expires;
        let mut until_ns = clock::stamp_ns(expires).ok_or(Error::Clock)?;
        let mut packets = Vec::with_capacity(4);
        for _ in 0..4 {
            if !permitted() {
                return Err(Error::Denied);
            }
            if Instant::now() >= until || clock::monotonic_ns().is_none_or(|now| now >= until_ns) {
                return Err(Error::Expired);
            }
            let mut pcm = [0_i16; 480];
            let (mut count, mut sequence, mut position, mut captured) = (0, 0, 0, 0);
            // SAFETY: C writes at most the supplied fixed sample capacity and
            // scalar outputs, retains no Rust address and serializes its ring.
            let available = status(unsafe {
                fr_macos_mic_read(
                    raw.as_ptr(),
                    pcm.as_mut_ptr(),
                    pcm.len(),
                    &raw mut count,
                    &raw mut sequence,
                    &raw mut position,
                    &raw mut captured,
                )
            })?;
            if !available {
                break;
            }
            if count != 480
                || captured == 0
                || self.next.is_some_and(|next| next != (sequence, position))
            {
                return Err(Error::CaptureDiscontinuity);
            }
            let native_until = captured.checked_add(100_000_000).ok_or(Error::Clock)?;
            until_ns = until_ns.min(native_until);
            let now = clock::monotonic_ns().ok_or(Error::Clock)?;
            if now < captured || now >= native_until {
                return Err(Error::Expired);
            }
            until = until.min(clock::local_instant(native_until).ok_or(Error::Clock)?);
            if !permitted() || Instant::now() >= until {
                return Err(Error::Expired);
            }
            let frame = AudioPcmFrame::from_interleaved(
                self.configuration.generation(),
                self.configuration.channels(),
                position,
                &pcm,
            )
            .map_err(|_| Error::Protocol)?;
            self.encoder.submit_pcm(&frame).map_err(|_| Error::Native)?;
            let packet = self
                .encoder
                .poll_packet()
                .map_err(|_| Error::Native)?
                .ok_or(Error::Native)?;
            pcm.fill(0);
            if !permitted()
                || Instant::now() >= until
                || clock::monotonic_ns().is_none_or(|now| now >= until_ns)
            {
                return Err(Error::Expired);
            }
            self.next = Some((
                sequence.checked_add(1).ok_or(Error::Clock)?,
                position.checked_add(480).ok_or(Error::Clock)?,
            ));
            packets.push(packet);
        }
        Ok(Batch {
            packets,
            expires: until,
            expires_ns: until_ns,
        })
    }
    pub fn close(&mut self) {
        self.encoder.close();
        if let Some(raw) = self.raw.take() {
            // SAFETY: ownership is consumed once. C either joins all callbacks
            // or retains a terminal tombstone until this supervised child exits.
            let _ = unsafe { fr_macos_mic_destroy(raw.as_ptr()) };
        }
    }
}
impl Drop for MicrophoneSource {
    fn drop(&mut self) {
        self.close();
    }
}
