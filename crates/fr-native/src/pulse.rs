//! Nonblocking local `PulseAudio` playback with explicit selection and bounded queues.
//!
//! Run on a supervised audio worker, never the input-authority thread or realtime
//! callback. The native mainloop is driven a bounded number of nonblocking turns;
//! it creates no Rust runtime or application thread. Native calls still require
//! process supervision for hangs. This is an output adapter, not audio permission.
//! Call only after local enable and observation approval. No capture/microphone,
//! automatic device selection, TCP server, reconnect or format fallback exists.

mod driver;
mod ffi;
mod output;

/// Real wire-to-codec-to-device receive owner.
pub mod playout;

use fr_client::{
    audio::playout::{AudioSubmission, PlayoutClock},
    input::ClientInstant,
};
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use std::{
    cell::Cell,
    ffi::{CString, c_void},
    path::Path,
    ptr::NonNull,
};

/// Per-stream native queue ceiling, verified against the server's actual reply.
pub const MAX_DEVICE_BUFFER_MS: u16 = 40;
/// Maximum output scheduling lead; the actual server-derived lead is fixed on first write.
pub const OUTPUT_LEAD_MS: u16 = 20;
const STARTUP_US: u64 = 2_000_000;
const STOP_US: u64 = 100_000;
const TIMING_AGE_US: u64 = 50_000;
const TIMING_PERIOD_US: u64 = 10_000;
const LEAD_SAMPLES: u64 = OUTPUT_LEAD_MS as u64 * 48;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Selection,
    Configuration,
    Allocation,
    Unavailable,
    Native,
    Pending,
    Closed,
    Denied,
    Expired,
    Clock,
    BufferLimit,
    DeviceChanged,
    Suspended,
    Backpressure,
    Metadata,
    SubmissionUnknown,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pulse-playback: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Locally chosen UNIX server and named output. Neither value comes from the
/// peer. No default sink aliases, server lists, TCP addresses or autospawn.
/// Debug intentionally omits local paths and device names.
pub struct Selection {
    server: CString,
    sink: CString,
}
impl Selection {
    pub fn new(server_socket: &Path, sink: &str) -> Result<Self, Error> {
        let socket = server_socket.to_str().ok_or(Error::Selection)?;
        if !server_socket.is_absolute()
            || socket.len() > 100
            || socket.len() < 2
            || socket.bytes().any(|b| b <= b' ' || b == 127 || b == b':')
            || sink.is_empty()
            || sink.len() > 255
            || !sink
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b))
        {
            return Err(Error::Selection);
        }
        Ok(Self {
            server: CString::new(format!("unix:{socket}")).map_err(|_| Error::Selection)?,
            sink: CString::new(sink).map_err(|_| Error::Selection)?,
        })
    }
}
impl std::fmt::Debug for Selection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PulseSelection([local UNIX server and output])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connecting,
    Ready,
    Stopping,
    Closed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// Server acknowledged cork then per-stream flush. Already delivered device
    /// samples cannot be retracted; this is not an audible-silence measurement.
    Flushed,
    /// Local admission is fenced and the native connection is gone. No flush
    /// acknowledgement was obtained; do not claim that it was.
    Disconnected,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submission {
    pub audio: AudioSubmission,
    pub scheduled_output_sample: u64,
    pub native_queue_capacity_bytes: u32,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Context,
    Stream,
    Uncork,
    Timing,
    Running,
    Cork,
    Flush,
    Closed,
}

struct Operation {
    ptr: NonNull<c_void>,
    reply: Box<Cell<i32>>,
    requested_at: u64,
}
impl Operation {
    fn result(&self) -> Result<Option<()>, Error> {
        // SAFETY: exclusive live operation; this query does not invoke callbacks.
        match unsafe { ffi::pa_operation_get_state(self.ptr.as_ptr()) } {
            0 => Ok(None),
            1 if self.reply.get() == 1 => Ok(Some(())),
            _ => Err(Error::Native),
        }
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        // SAFETY: cancel prevents any later callback before the boxed userdata
        // dies; our nonthreaded mainloop cannot concurrently dispatch a callback.
        unsafe {
            ffi::pa_operation_cancel(self.ptr.as_ptr());
            ffi::pa_operation_unref(self.ptr.as_ptr());
        }
    }
}
unsafe extern "C" fn success(_: *mut c_void, result: i32, data: *mut c_void) {
    // SAFETY: a live Operation owns this stable Box<Cell<i32>> for the entire
    // callback lifetime. No Rust reference to the cell is exclusive, no unwind.
    let reply = unsafe { &*data.cast::<Cell<i32>>() };
    reply.set(if result != 0 { 1 } else { -1 });
}

/// Unique !Send/!Sync owner of a native connection and one playback stream.
/// A terminal owner never reopens. Replacement requires a new admitted epoch in
/// the containing session, not numeric generation reuse in a new constructor.
pub struct PlaybackDevice {
    mainloop: Option<NonNull<c_void>>,
    context: Option<NonNull<c_void>>,
    stream: Option<NonNull<c_void>>,
    operation: Option<Operation>,
    selection: Selection,
    config: AudioStreamConfig,
    stage: Stage,
    deadline: u64,
    last_now: u64,
    timing_at: Option<u64>,
    clock_probe: Option<(u64, u64)>,
    last_samples: u64,
    device_index: Option<u32>,
    queue_bytes: u32,
    next: Option<(u64, u64)>,
    lead_samples: Option<u64>,
    error: Option<Error>,
    stopped: Option<StopOutcome>,
}
impl std::fmt::Debug for PlaybackDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PulsePlaybackDevice")
            .field("state", &self.state())
            .field("queue_bytes", &self.queue_bytes)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}
impl PlaybackDevice {
    /// Initiate silent native configuration after local enable/approval. The
    /// caller must continue servicing permission/revoke while polling startup.
    pub fn connect(
        selection: Selection,
        config: AudioStreamConfig,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        // The 5 ms codec profile remains available independently. This native
        // output path does not qualify its stricter scheduling envelope; refuse
        // before connecting rather than silently repacketize or widen a slot.
        if config.direction() != AudioDirection::Downlink
            || !matches!(config.frame_duration_ms(), 10 | 20)
        {
            return Err(Error::Configuration);
        }
        let deadline = now.0.checked_add(STARTUP_US).ok_or(Error::Clock)?;
        let mut owner = Self {
            mainloop: None,
            context: None,
            stream: None,
            operation: None,
            selection,
            config,
            stage: Stage::Context,
            deadline,
            last_now: now.0,
            timing_at: None,
            clock_probe: None,
            last_samples: 0,
            device_index: None,
            queue_bytes: 0,
            next: None,
            lead_samples: None,
            error: None,
            stopped: None,
        };
        // SAFETY: each successful allocation is immediately installed in the
        // sole Drop owner, including every partial-construction failure path.
        unsafe {
            owner.mainloop = Some(NonNull::new(ffi::pa_mainloop_new()).ok_or(Error::Allocation)?);
            let api = ffi::pa_mainloop_get_api(owner.mainloop.expect("installed").as_ptr());
            if api.is_null() {
                return Err(Error::Native);
            }
            owner.context = Some(
                NonNull::new(ffi::pa_context_new(api, c"FrankenRemote playback".as_ptr()))
                    .ok_or(Error::Allocation)?,
            );
            // PA_CONTEXT_NOAUTOSPAWN = 1. Connect only to the explicit local socket.
            if ffi::pa_context_connect(
                owner.context.expect("installed").as_ptr(),
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
    pub const fn configuration(&self) -> AudioStreamConfig {
        self.config
    }
    pub const fn error(&self) -> Option<Error> {
        self.error
    }
    pub const fn stop_outcome(&self) -> Option<StopOutcome> {
        self.stopped
    }
    pub const fn queue_capacity_bytes(&self) -> u32 {
        self.queue_bytes
    }
    pub const fn state(&self) -> State {
        match self.stage {
            Stage::Running => State::Ready,
            Stage::Cork | Stage::Flush => State::Stopping,
            Stage::Closed => State::Closed,
            _ => State::Connecting,
        }
    }
    fn stream(&self) -> Result<*mut c_void, Error> {
        self.stream.map(NonNull::as_ptr).ok_or(Error::Pending)
    }
    fn advance_time(&mut self, now: ClientInstant) -> Result<(), Error> {
        if now.0 < self.last_now {
            return Err(Error::Clock);
        }
        self.last_now = now.0;
        if self.stage == Stage::Closed {
            return Err(self.error.unwrap_or(Error::Closed));
        }
        if self.stage != Stage::Running && now.0 >= self.deadline {
            return Err(Error::Expired);
        }
        Ok(())
    }
}

impl Drop for PlaybackDevice {
    fn drop(&mut self) {
        self.disconnect();
    }
}
struct Guard<'a> {
    device: &'a mut PlaybackDevice,
    completed: bool,
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.device.fail(Error::Closed);
        }
    }
}

#[cfg(test)]
mod tests;
