//! Nonblocking PulseAudio RECORD of one locally selected playback monitor.
//!
//! Host playback capture (plan §15.4: "a selected output monitor through
//! qualified Linux audio APIs"). Runs only in the supervised audio worker
//! process, never in frd. The server and monitor are local configuration; a
//! peer never names a device. The stream records 48 kHz native-endian s16 in
//! the negotiated channel count: the audio server converts/resamples the
//! monitor's own format at this one boundary. The record queue is bounded in
//! the server; a reader that falls behind loses the OLDEST audio there, it is
//! never accumulated here. No microphone, autospawn, TCP server or fallback.
use super::{Error, ffi};
use fr_core::audio::AudioChannels;
use fr_media::worker::audio::Monitor;
use std::{
    ffi::{CString, c_void},
    path::Path,
    ptr::NonNull,
};

const STARTUP_US: u64 = 2_000_000;
/// Server-side record queue bound.
pub const RECORD_QUEUE_MS: u32 = 100;
/// Requested fragment (and latency target) of the record stream.
pub const FRAGMENT_MS: u32 = 10;
/// Nonblocking native turns per read: enough to drain one full record queue
/// of 5 ms server fragments plus control events, never an unbounded loop.
const MAX_READ_TURNS: usize = 48;

/// Locally chosen UNIX server and playback monitor. Debug omits both.
pub struct CaptureSelection {
    server: CString,
    source: CString,
}
impl CaptureSelection {
    pub fn new(server_socket: &Path, monitor: &Monitor) -> Result<Self, Error> {
        let socket = server_socket.to_str().ok_or(Error::Selection)?;
        if !server_socket.is_absolute()
            || socket.len() > 100
            || socket.len() < 2
            || socket.bytes().any(|b| b <= b' ' || b == 127 || b == b':')
        {
            return Err(Error::Selection);
        }
        let source = match monitor {
            // The server resolves its current default sink's monitor ONCE at
            // connect; DONT_MOVE pins it, so a later default never follows.
            Monitor::DefaultSink => "@DEFAULT_MONITOR@".to_owned(),
            Monitor::Sink(sink) => {
                if sink.is_empty()
                    || sink.len() > 247
                    || !sink
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
                {
                    return Err(Error::Selection);
                }
                format!("{sink}.monitor")
            }
        };
        Ok(Self {
            server: CString::new(format!("unix:{socket}")).map_err(|_| Error::Selection)?,
            source: CString::new(source).map_err(|_| Error::Selection)?,
        })
    }
}
impl std::fmt::Debug for CaptureSelection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PulseCaptureSelection([local UNIX server and monitor])")
    }
}

/// One drained piece of the record stream.
#[derive(Clone, Copy)]
pub enum Chunk<'a> {
    /// Interleaved native-endian s16 bytes (not necessarily frame aligned).
    Bytes(&'a [u8]),
    /// An explicit server-reported discontinuity of this many bytes.
    Hole(usize),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Context,
    Stream,
    Ready,
    Closed,
}

/// Unique !Send/!Sync owner of one native connection and one record stream.
pub struct CaptureDevice {
    mainloop: Option<NonNull<c_void>>,
    context: Option<NonNull<c_void>>,
    stream: Option<NonNull<c_void>>,
    selection: CaptureSelection,
    channels: AudioChannels,
    stage: Stage,
    deadline: u64,
    last_now: u64,
    queue_bytes: u32,
    device_index: Option<u32>,
    error: Option<Error>,
}
impl std::fmt::Debug for CaptureDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PulseCaptureDevice")
            .field("ready", &(self.stage == Stage::Ready))
            .field("queue_bytes", &self.queue_bytes)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl CaptureDevice {
    /// Start a silent native connection; `poll` completes it within 2 s.
    pub fn connect(
        selection: CaptureSelection,
        channels: AudioChannels,
        now_us: u64,
    ) -> Result<Self, Error> {
        let mut owner = Self {
            mainloop: None,
            context: None,
            stream: None,
            selection,
            channels,
            stage: Stage::Context,
            deadline: now_us.checked_add(STARTUP_US).ok_or(Error::Clock)?,
            last_now: now_us,
            queue_bytes: 0,
            device_index: None,
            error: None,
        };
        // SAFETY: each allocation is installed in the sole Drop owner at once,
        // including every partial-construction failure path.
        unsafe {
            let mainloop = NonNull::new(ffi::pa_mainloop_new()).ok_or(Error::Allocation)?;
            owner.mainloop = Some(mainloop);
            let api = ffi::pa_mainloop_get_api(mainloop.as_ptr());
            if api.is_null() {
                return Err(Error::Native);
            }
            let context = NonNull::new(ffi::pa_context_new(api, c"FrankenRemote capture".as_ptr()))
                .ok_or(Error::Allocation)?;
            owner.context = Some(context);
            // PA_CONTEXT_NOAUTOSPAWN = 1: only the explicit local socket.
            if ffi::pa_context_connect(
                context.as_ptr(),
                owner.selection.server.as_ptr(),
                1,
                std::ptr::null(),
            ) < 0
            {
                return Err(Error::Unavailable);
            }
        }
        Ok(owner)
    }
    pub const fn is_ready(&self) -> bool {
        matches!(self.stage, Stage::Ready)
    }
    pub const fn error(&self) -> Option<Error> {
        self.error
    }
    /// The server's actual record queue bound, once ready.
    pub const fn queue_bytes(&self) -> u32 {
        self.queue_bytes
    }
    fn stride(&self) -> u32 {
        u32::from(self.channels.count()) * 2
    }
    fn advance(&mut self, now_us: u64) -> Result<(), Error> {
        if now_us < self.last_now {
            return Err(Error::Clock);
        }
        self.last_now = now_us;
        if self.stage == Stage::Closed {
            return Err(self.error.unwrap_or(Error::Closed));
        }
        if self.stage != Stage::Ready && now_us >= self.deadline {
            return Err(Error::Expired);
        }
        Ok(())
    }
    /// One nonblocking native turn; the number of dispatched event sources.
    fn iterate(&mut self) -> Result<u32, Error> {
        let mainloop = self.mainloop.ok_or(Error::Closed)?;
        // SAFETY: live exclusive loop, polled mode (block=0); no application
        // callback is installed on this owner.
        let dispatched =
            unsafe { ffi::pa_mainloop_iterate(mainloop.as_ptr(), 0, std::ptr::null_mut()) };
        u32::try_from(dispatched).map_err(|_| Error::Native)
    }
    /// At most four nonblocking native turns. Returns true once recording.
    pub fn poll(&mut self, now_us: u64) -> Result<bool, Error> {
        let result = self.poll_inner(now_us);
        if let Err(error) = result {
            self.fail(error);
        }
        result
    }
    fn poll_inner(&mut self, now_us: u64) -> Result<bool, Error> {
        for _ in 0..4 {
            self.advance(now_us)?;
            self.iterate()?;
            self.progress()?;
            if self.stage == Stage::Ready {
                break;
            }
        }
        Ok(self.stage == Stage::Ready)
    }
    fn progress(&mut self) -> Result<(), Error> {
        let context = self.context.ok_or(Error::Closed)?;
        // SAFETY: owned handle, read-only state query.
        let state = unsafe { ffi::pa_context_get_state(context.as_ptr()) };
        if state > ffi::CONTEXT_READY {
            return Err(Error::Unavailable);
        }
        match self.stage {
            Stage::Context if state == ffi::CONTEXT_READY => self.connect_stream(),
            Stage::Stream | Stage::Ready => {
                let stream = self.stream.ok_or(Error::Closed)?;
                // SAFETY: owned handle, read-only state query.
                let stream_state = unsafe { ffi::pa_stream_get_state(stream.as_ptr()) };
                if stream_state > ffi::STREAM_READY {
                    // Missing monitor, removed sink or a killed stream.
                    return Err(if self.stage == Stage::Stream {
                        Error::Unavailable
                    } else {
                        Error::DeviceChanged
                    });
                }
                if stream_state == ffi::STREAM_READY {
                    self.validate()?;
                    self.stage = Stage::Ready;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    fn connect_stream(&mut self) -> Result<(), Error> {
        let spec = ffi::SampleSpec {
            format: ffi::S16_NATIVE,
            rate: 48_000,
            channels: self.channels.count(),
        };
        let stride = self.stride();
        let attr = ffi::BufferAttr {
            maxlength: RECORD_QUEUE_MS * 48 * stride,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: FRAGMENT_MS * 48 * stride,
        };
        // SAFETY: valid public C layouts and borrowed NUL-terminated names; C
        // copies them before returning. No format-fix flags: the server
        // converts to exactly this spec or the stream fails.
        unsafe {
            self.stream = Some(
                NonNull::new(ffi::pa_stream_new(
                    self.context.ok_or(Error::Closed)?.as_ptr(),
                    c"Remote playback capture".as_ptr(),
                    &raw const spec,
                    std::ptr::null(),
                ))
                .ok_or(Error::Allocation)?,
            );
            if ffi::pa_stream_connect_record(
                self.stream.ok_or(Error::Closed)?.as_ptr(),
                self.selection.source.as_ptr(),
                &raw const attr,
                ffi::ADJUST_LATENCY | ffi::DONT_MOVE,
            ) < 0
            {
                return Err(Error::Unavailable);
            }
        }
        self.stage = Stage::Stream;
        Ok(())
    }
    fn validate(&mut self) -> Result<(), Error> {
        let stream = self.stream.ok_or(Error::Closed)?;
        // SAFETY: returned value pointers are owned by the live stream; copy
        // immediately, nothing escapes a mainloop turn.
        let (spec, attr, index) = unsafe {
            let spec = ffi::pa_stream_get_sample_spec(stream.as_ptr())
                .as_ref()
                .ok_or(Error::Native)?;
            let attr = ffi::pa_stream_get_buffer_attr(stream.as_ptr())
                .as_ref()
                .ok_or(Error::Native)?;
            (
                *spec,
                *attr,
                ffi::pa_stream_get_device_index(stream.as_ptr()),
            )
        };
        let stride = self.stride();
        if spec.format != ffi::S16_NATIVE
            || spec.rate != 48_000
            || spec.channels != self.channels.count()
        {
            return Err(Error::Configuration);
        }
        if attr.maxlength == 0
            || attr.maxlength > RECORD_QUEUE_MS * 48 * stride
            || attr.fragsize == 0
            || attr.fragsize > attr.maxlength
            || self.queue_bytes != 0 && self.queue_bytes != attr.maxlength
        {
            return Err(Error::BufferLimit);
        }
        if index == u32::MAX || self.device_index.is_some_and(|old| old != index) {
            return Err(Error::DeviceChanged);
        }
        self.device_index = Some(index);
        self.queue_bytes = attr.maxlength;
        Ok(())
    }
    /// Drain at most `max_bytes` already recorded bytes, oldest first, into
    /// `sink`. Two nonblocking native turns fetch what the server has sent.
    /// Returns (bytes delivered, whether the queue was found full).
    pub fn read(
        &mut self,
        now_us: u64,
        max_bytes: usize,
        mut sink: impl FnMut(Chunk<'_>) -> Result<(), Error>,
    ) -> Result<(usize, bool), Error> {
        let result = self.read_inner(now_us, max_bytes, &mut sink);
        if let Err(error) = result {
            self.fail(error);
        }
        result
    }
    fn read_inner(
        &mut self,
        now_us: u64,
        max_bytes: usize,
        sink: &mut impl FnMut(Chunk<'_>) -> Result<(), Error>,
    ) -> Result<(usize, bool), Error> {
        self.advance(now_us)?;
        if self.stage != Stage::Ready {
            return Err(Error::Pending);
        }
        // Fetch everything the server already sent: nonblocking turns until
        // one dispatches nothing, bounded so a flooding server can never keep
        // this call busy. Each turn may move only one server fragment.
        let stream = self.stream.ok_or(Error::Closed)?.as_ptr();
        for _ in 0..MAX_READ_TURNS {
            if self.iterate()? == 0 {
                break;
            }
        }
        self.progress()?;
        // SAFETY: owned live stream; read-only size query.
        let readable = unsafe { ffi::pa_stream_readable_size(stream) };
        if readable == usize::MAX {
            return Err(Error::Native);
        }
        let full = u32::try_from(readable).map_or(true, |n| {
            n.saturating_add(FRAGMENT_MS * 48 * self.stride()) >= self.queue_bytes
        });
        let mut delivered = 0_usize;
        while delivered < max_bytes {
            let mut data: *const c_void = std::ptr::null();
            let mut bytes = 0_usize;
            // SAFETY: out-pointers are valid locals. The returned fragment is
            // owned by the stream until pa_stream_drop; it is only borrowed
            // for the synchronous sink call below and never retained.
            if unsafe { ffi::pa_stream_peek(stream, &raw mut data, &raw mut bytes) } < 0 {
                return Err(Error::Native);
            }
            if bytes == 0 {
                break;
            }
            let chunk = if data.is_null() {
                Chunk::Hole(bytes)
            } else {
                // SAFETY: libpulse guarantees `bytes` readable bytes at `data`
                // until the matching drop; the slice dies before that call.
                Chunk::Bytes(unsafe { std::slice::from_raw_parts(data.cast::<u8>(), bytes) })
            };
            let result = sink(chunk);
            // SAFETY: exactly one drop per successful non-empty peek.
            if unsafe { ffi::pa_stream_drop(stream) } < 0 {
                return Err(Error::Native);
            }
            result?;
            delivered = delivered.saturating_add(bytes);
        }
        Ok((delivered, full))
    }
    /// Immediate teardown; recording stops and nothing is retained.
    pub fn disconnect(&mut self) {
        self.stage = Stage::Closed;
        // SAFETY: no callback can run concurrently; disconnect/unref stream,
        // then context, then free the loop storage last.
        unsafe {
            if let Some(stream) = self.stream.take() {
                let _ = ffi::pa_stream_disconnect(stream.as_ptr());
                ffi::pa_stream_unref(stream.as_ptr());
            }
            if let Some(context) = self.context.take() {
                ffi::pa_context_disconnect(context.as_ptr());
                ffi::pa_context_unref(context.as_ptr());
            }
            if let Some(mainloop) = self.mainloop.take() {
                ffi::pa_mainloop_free(mainloop.as_ptr());
            }
        }
    }
    fn fail(&mut self, error: Error) {
        if error == Error::Pending {
            return;
        }
        self.error.get_or_insert(error);
        self.disconnect();
    }
}
impl Drop for CaptureDevice {
    fn drop(&mut self) {
        self.disconnect();
    }
}
