//! Explicit, thread-confined `PipeWire` virtual microphone for a supervised native
//! audio worker. The application selects its Unix socket and supplies the
//! original session's permission check. This is not a runtime, input grant,
//! system default-device change, microphone capture source, or network listener.
//!
//! PCM is bounded to four 10/20-ms blocks. Every retained block keeps its original
//! deadline, checked again in the actual `PipeWire` callback alongside the
//! original permission check. Native callbacks run only during `poll`; an
//! independent `Control::stop`
//! fences the next submission even when the worker has not serviced its queue.
//! Bytes already submitted to the OS cannot be recalled.
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    marker::PhantomData,
    os::{fd::IntoRawFd, unix::net::UnixStream},
    panic::{AssertUnwindSafe, catch_unwind},
    path::{Component, Path},
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc, Once,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub const MAX_QUEUED_BLOCKS: usize = 4;
pub const MAX_BLOCK_FRAMES: usize = 960;
pub const MAX_RETAINED_AGE: Duration = Duration::from_millis(100);
const STARTUP: Duration = Duration::from_secs(2);
static INITIALIZE: Once = Once::new();
static IDENTIFIER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    InvalidConfiguration,
    InvalidServer,
    Unavailable,
    StartupTimeout,
    PermissionEnded,
    Closed,
    Generation,
    Format,
    Timeline,
    Expired,
    Backpressure,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pipewire-microphone: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Connecting,
    /// The source has a real `PipeWire` node ID, not proof an app selected it.
    Ready,
    Closed,
}

/// Content-free counters. Submitted means queued to `PipeWire`, never recorded by
/// an application. The queue includes four fixed PCM blocks; native buffers are
/// separately bounded to two 4096-frame buffers in the C boundary.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Statistics {
    pub submitted_frames: u64,
    pub silent_frames: u64,
    pub expired_frames: u64,
    pub queued_frames: u32,
    pub node_id: u32,
}

struct Gate {
    origin: Instant,
    stopped: AtomicBool,
}
/// Terminal, thread-safe stop of this exact endpoint. A clone cannot keep its
/// `PipeWire` connection alive, re-enable it, or address a replacement generation.
#[derive(Clone)]
pub struct Control(Arc<Gate>);
impl Control {
    pub fn stop(&self) {
        self.0.stopped.store(true, Ordering::Release);
    }
    pub fn is_stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }
}
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneControl")
            .field("stopped", &self.is_stopped())
            .finish()
    }
}

#[repr(C)]
struct Native {
    _private: [u8; 0],
}
type Clock = unsafe extern "C" fn(*const c_void, *mut u64) -> c_int;
type Permission = unsafe extern "C" fn(*mut c_void) -> c_int;
unsafe extern "C" {
    fn fr_pipewire_mic_init();
    fn fr_pipewire_mic_new(
        fd: c_int,
        name: *const c_char,
        channels: u32,
        clock: Clock,
        context: *const c_void,
        out: *mut *mut Native,
    ) -> c_int;
    fn fr_pipewire_mic_poll(
        mic: *mut Native,
        permission: Permission,
        context: *mut c_void,
    ) -> c_int;
    fn fr_pipewire_mic_submit(mic: *mut Native, pcm: *const i16, frames: u32, until: u64) -> c_int;
    fn fr_pipewire_mic_statistics(mic: *const Native, out: *mut Statistics);
    fn fr_pipewire_mic_free(mic: *mut Native);
}
unsafe extern "C" fn clock(context: *const c_void, now: *mut u64) -> c_int {
    // SAFETY: the endpoint retains this Arc allocation through native destroy.
    // The native callback passes a writable u64 and never retains that pointer.
    let gate = unsafe { &*context.cast::<Gate>() };
    let Ok(elapsed) = u64::try_from(gate.origin.elapsed().as_micros()) else {
        return 0;
    };
    // SAFETY: writable output supplied synchronously by the native boundary.
    unsafe { *now = elapsed };
    c_int::from(!gate.stopped.load(Ordering::Acquire))
}
unsafe extern "C" fn permission<P: FnMut() -> bool>(context: *mut c_void) -> c_int {
    // SAFETY: poll exclusively borrows this closure for its synchronous native
    // call, on this thread only. C clears the pointer before every return. No
    // callback can run on a native realtime thread or reenter this same owner.
    let permitted = unsafe { &mut *context.cast::<P>() };
    // Unwinding through C is forbidden. A panicking authority check denies this
    // exact endpoint terminally, like any other failed original permission.
    c_int::from(catch_unwind(AssertUnwindSafe(permitted)).unwrap_or(false))
}

/// One original audio generation on one explicitly selected local `PipeWire`
/// connection. Construct/poll/drop ONLY on a supervised native worker thread.
/// No `Send`/`Sync`: callbacks and `PipeWire` ownership remain thread-confined.
pub struct Microphone {
    native: Option<NonNull<Native>>,
    control: Control,
    config: AudioStreamConfig,
    state: State,
    until: Instant,
    last_end: Option<u64>,
    statistics: Statistics,
    _thread: PhantomData<Rc<()>>,
}
impl std::fmt::Debug for Microphone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireMicrophone")
            .field("state", &self.state)
            .field("stopped", &self.control.is_stopped())
            .finish_non_exhaustive()
    }
}
impl Microphone {
    /// Create a local source only after explicit session enable/approval. The
    /// callback must check that original permission, not just connection liveness.
    /// The actual connected daemon must have the same OS UID as this process.
    /// No socket environment fallback, persistent module, or default-device
    /// metadata is used. Native setup still needs external worker supervision.
    pub fn connect(
        server: &Path,
        config: AudioStreamConfig,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Self, Error> {
        validate_configuration(config)?;
        if !server.is_absolute()
            || server
                .components()
                .any(|c| matches!(c, Component::ParentDir))
        {
            return Err(Error::InvalidServer);
        }
        if !permitted() {
            return Err(Error::PermissionEnded);
        }
        let origin = Instant::now();
        let until = origin.checked_add(STARTUP).ok_or(Error::Unavailable)?;
        let control = Control(Arc::new(Gate {
            origin,
            stopped: AtomicBool::new(false),
        }));
        let socket = UnixStream::connect(server).map_err(|_| Error::Unavailable)?;
        if !permitted() {
            return Err(Error::PermissionEnded);
        }
        if Instant::now() >= until {
            return Err(Error::StartupTimeout);
        }
        let id = IDENTIFIER
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| Error::Unavailable)?;
        let name = CString::new(format!(
            "frankenremote.microphone.{}.{id}",
            std::process::id()
        ))
        .map_err(|_| Error::Unavailable)?;
        INITIALIZE.call_once(|| {
            // SAFETY: called once per process before any of our native contexts.
            // No pw_deinit can invalidate another live source's global library.
            unsafe { fr_pipewire_mic_init() };
        });
        let mut pointer = std::ptr::null_mut();
        // SAFETY: fd ownership transfers on every native return path. The name
        // lives through setup; C copies it. Arc's stable Gate allocation is
        // retained until after free; all callbacks use only its immutable clock
        // and atomic fence. The out pointer is a unique writable stack slot.
        let result = unsafe {
            fr_pipewire_mic_new(
                socket.into_raw_fd(),
                name.as_ptr(),
                u32::from(config.channels().count()),
                clock,
                Arc::as_ptr(&control.0).cast(),
                &raw mut pointer,
            )
        };
        if result != 0 {
            return Err(Error::Unavailable);
        }
        let native = NonNull::new(pointer).ok_or(Error::Unavailable)?;
        let mut owner = Self {
            native: Some(native),
            control,
            config,
            state: State::Connecting,
            until,
            last_end: None,
            statistics: Statistics::default(),
            _thread: PhantomData,
        };
        owner.check_permission(&mut permitted)?;
        if Instant::now() >= until {
            owner.close();
            return Err(Error::StartupTimeout);
        }
        Ok(owner)
    }
    pub fn control(&self) -> Control {
        self.control.clone()
    }
    pub const fn state(&self) -> State {
        self.state
    }
    pub const fn configuration(&self) -> AudioStreamConfig {
        self.config
    }
    /// Drive one nonblocking native main-loop iteration on the creating thread.
    /// The original permission check also runs immediately before native PCM
    /// publication. It must be nonblocking and avoid allocation or I/O. A panic
    /// during a native callback is caught and treated as terminal permission loss.
    /// This is the only method which processes native callbacks.
    pub fn poll<P: FnMut() -> bool>(&mut self, mut permitted: P) -> Result<State, Error> {
        self.check_permission(&mut permitted)?;
        if self.state == State::Connecting && Instant::now() >= self.until {
            self.close();
            return Err(Error::StartupTimeout);
        }
        let native = self.native.ok_or(Error::Closed)?;
        // SAFETY: this unique thread-confined native context remains live;
        // callback storage is retained by self for the entire call. The scoped
        // closure pointer is erased by C before return; no RT_PROCESS is enabled.
        let result = unsafe {
            fr_pipewire_mic_poll(
                native.as_ptr(),
                permission::<P>,
                (&raw mut permitted).cast(),
            )
        };
        if result == 2 {
            self.close();
            return Err(Error::PermissionEnded);
        }
        self.check_permission(&mut permitted)?;
        if self.state == State::Connecting && Instant::now() >= self.until {
            self.close();
            return Err(Error::StartupTimeout);
        }
        self.state = match result {
            0 => State::Connecting,
            1 => State::Ready,
            _ => {
                self.close();
                return Err(Error::Unavailable);
            }
        };
        Ok(self.state)
    }
    /// Admit one decoded frame with its ORIGINAL local deadline, at most 100 ms
    /// away. Use the same audio generation/channels as the original configuration.
    /// A valid attempt consumes its sample timeline even when expired or full:
    /// callers discard that sound, never retry it with a fresh deadline.
    /// Success means bounded local custody; inspect counters for OS submission.
    pub fn submit(
        &mut self,
        frame: &AudioPcmFrame,
        expires: Instant,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.check_permission(&mut permitted)?;
        if self.state != State::Ready {
            return Err(Error::Unavailable);
        }
        let end = validate_frame(self.config, frame, self.last_end)?;
        let now = Instant::now();
        if expires > now.checked_add(MAX_RETAINED_AGE).ok_or(Error::Expired)? {
            return Err(Error::InvalidConfiguration);
        }
        self.last_end = Some(end);
        let until = expires
            .checked_duration_since(self.control.0.origin)
            .and_then(|d| u64::try_from(d.as_micros()).ok())
            .filter(|_| now < expires)
            .ok_or(Error::Expired)?;
        self.check_permission(&mut permitted)?;
        let native = self.native.ok_or(Error::Closed)?;
        // SAFETY: PCM has exactly the checked channels and <=960 frames; its
        // initialized sample slice lives through this copying call. The native
        // owner retains the ORIGINAL absolute deadline, never this Rust borrow.
        match unsafe {
            fr_pipewire_mic_submit(
                native.as_ptr(),
                frame.samples().as_ptr(),
                u32::try_from(frame.samples_per_channel()).map_err(|_| Error::Format)?,
                until,
            )
        } {
            0 => Ok(()),
            1 => Err(Error::Backpressure),
            2 => Err(Error::Expired),
            _ => {
                self.close();
                Err(Error::Unavailable)
            }
        }
    }
    pub fn statistics(&self) -> Statistics {
        let mut result = self.statistics;
        if let Some(native) = self.native {
            // SAFETY: read of this same thread's live context into a fixed ABI
            // output; no callback or other thread mutates its native counters.
            unsafe { fr_pipewire_mic_statistics(native.as_ptr(), &raw mut result) };
        }
        result
    }
    /// Fence first, erase all private PCM, then destroy this exact stream and
    /// connection. Destruction never changes other sources or the OS default.
    /// Server-side removal is asynchronous; this is local cleanup evidence only.
    pub fn close(&mut self) {
        self.control.stop();
        self.statistics = self.statistics();
        self.statistics.queued_frames = 0;
        if let Some(native) = self.native.take() {
            // SAFETY: the original unique context is destroyed on its creating
            // thread; Gate storage outlives destruction and all callbacks.
            unsafe { fr_pipewire_mic_free(native.as_ptr()) };
        }
        self.state = State::Closed;
    }
    fn check_permission(&mut self, permitted: &mut impl FnMut() -> bool) -> Result<(), Error> {
        if self.native.is_none() {
            return Err(Error::Closed);
        }
        if self.control.is_stopped() || !permitted() {
            self.close();
            return Err(Error::PermissionEnded);
        }
        Ok(())
    }
}
impl Drop for Microphone {
    fn drop(&mut self) {
        self.close();
    }
}
fn validate_configuration(config: AudioStreamConfig) -> Result<(), Error> {
    if config.direction() != AudioDirection::Uplink
        || !matches!(config.frame_duration_ms(), 10 | 20)
    {
        return Err(Error::InvalidConfiguration);
    }
    Ok(())
}
fn validate_frame(
    config: AudioStreamConfig,
    frame: &AudioPcmFrame,
    last_end: Option<u64>,
) -> Result<u64, Error> {
    if frame.generation() != config.generation() {
        return Err(Error::Generation);
    }
    if frame.channels() != config.channels()
        || frame.sample_rate() != 48_000
        || frame.samples_per_channel() != usize::from(config.frame_duration_ms()) * 48
        || frame.samples_per_channel() > MAX_BLOCK_FRAMES
    {
        return Err(Error::Format);
    }
    if last_end.is_some_and(|floor| frame.timestamp_samples() < floor) {
        return Err(Error::Timeline);
    }
    frame
        .timestamp_samples()
        .checked_add(u64::try_from(frame.samples_per_channel()).map_err(|_| Error::Format)?)
        .ok_or(Error::Timeline)
}
