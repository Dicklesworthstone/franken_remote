//! Bounded, worker-confined Core Audio PCM output for an explicitly enabled
//! remote downlink. Opus remains in its separately supervised decoder process.
//!
//! The default local output is resolved and pinned once. Device/format changes,
//! stalled clocks and late sound retire this owner. Native callbacks provide
//! allocation credit only; actual Audio Queue sample time paces playback.

use crate::macos_audio_policy::{CLOCK_STALL_US, Clock, Window};
pub use crate::macos_audio_policy::{Error, STARTUP_US};
use fr_client::{
    audio::playout::{AudioSubmission, PlayoutClock},
    input::ClientInstant,
};
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use std::{
    ffi::{c_int, c_void},
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind},
    ptr::NonNull,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

/// Shared acknowledgement of actual native callback/queue disposal. A failed
/// disposal remains incomplete, including after Drop; it is never a new grant.
#[derive(Clone)]
pub struct Retirement(Arc<AtomicBool>);
impl Retirement {
    pub fn is_complete(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Only the locally selected default output. Remote records cannot name an
/// output device or cause routing to a different speaker after configuration.
#[derive(Debug)]
pub struct Selection(());
impl Selection {
    #[allow(clippy::unnecessary_wraps)] // same fallible selection seam as native Linux output
    pub fn default_output() -> Result<Self, Error> {
        Ok(Self(()))
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
    /// Immediate stop/reset and callback disposal succeeded for this queue.
    /// Already delivered samples cannot be retracted; this is not audibility.
    Flushed,
    /// Local submission ended. This alone is not a native disposal witness.
    Disconnected,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submission {
    pub audio: AudioSubmission,
    pub scheduled_output_sample: u64,
    pub native_queue_capacity_bytes: u32,
}

#[repr(C)]
#[derive(Default)]
struct NativeClock {
    samples: u64,
    lead: u64,
}
unsafe extern "C" {
    fn fr_macos_audio_create(channels: u32, largest_frames: u32, out: *mut *mut c_void) -> c_int;
    fn fr_macos_audio_clock(handle: *mut c_void, clock: *mut NativeClock) -> c_int;
    fn fr_macos_audio_submit(
        handle: *mut c_void,
        pcm: *const i16,
        samples: u32,
        start: u64,
        end: u64,
        permission: unsafe extern "C" fn(*mut c_void, u64) -> c_int,
        context: *mut c_void,
    ) -> c_int;
    fn fr_macos_audio_stop(handle: *mut c_void) -> c_int;
    fn fr_macos_audio_destroy(handle: *mut c_void) -> c_int;
}

/// Unique !Send/!Sync owner, created and retired inside the existing bounded
/// audio worker. The C object outlives every native callback. No callback keeps
/// a Rust reference or performs a decoder/session operation.
pub struct PlaybackDevice {
    handle: Option<NonNull<c_void>>,
    configuration: AudioStreamConfig,
    state: State,
    clock: Clock,
    reading: Option<PlayoutClock>,
    lead: Option<u64>,
    next: Option<(u64, u64)>,
    stopped: Option<StopOutcome>,
    error: Option<Error>,
    retirement: Retirement,
    confined: PhantomData<Rc<()>>,
}
impl std::fmt::Debug for PlaybackDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MacAudioPlaybackDevice")
            .field("state", &self.state)
            .field("error", &self.error)
            .field("retired", &self.retirement.is_complete())
            .finish_non_exhaustive()
    }
}
impl PlaybackDevice {
    /// Begin silent setup on the native worker. A successful allocation is not
    /// readiness: poll must observe a genuinely advancing native timeline.
    pub fn connect(
        selection: Selection,
        configuration: AudioStreamConfig,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        Self::connect_retained(selection, configuration, now).0
    }

    /// Retain the original native cleanup witness even when setup fails after
    /// allocating a queue. Session owners use this constructor so unsuccessful
    /// creation followed by unresolved disposal cannot count as retired.
    pub fn connect_retained(
        selection: Selection,
        configuration: AudioStreamConfig,
        now: ClientInstant,
    ) -> (Result<Self, Error>, Retirement) {
        let retirement = Retirement(Arc::new(AtomicBool::new(true)));
        let result = Self::create(selection, configuration, now, retirement.clone());
        (result, retirement)
    }

    fn create(
        _selection: Selection,
        configuration: AudioStreamConfig,
        now: ClientInstant,
        retirement: Retirement,
    ) -> Result<Self, Error> {
        if configuration.direction() != AudioDirection::Downlink
            || configuration.generation().as_raw() == 0
            || !matches!(configuration.frame_duration_ms(), 10 | 20)
        {
            return Err(Error::Configuration);
        }
        let nominal = configuration.expected_samples_per_frame();
        let largest = nominal
            .checked_add(nominal.div_ceil(1000))
            .ok_or(Error::Configuration)?;
        let mut handle = std::ptr::null_mut();
        // SAFETY: C copies the checked format scalars and writes one owned opaque
        // pointer. Its returned allocation, even on failure, belongs to us.
        let code = unsafe {
            fr_macos_audio_create(
                u32::from(configuration.channels().count()),
                largest,
                &raw mut handle,
            )
        };
        retirement.0.store(handle.is_null(), Ordering::Release);
        let mut device = Self {
            handle: NonNull::new(handle),
            configuration,
            state: State::Connecting,
            clock: Clock::new(now.0),
            reading: None,
            lead: None,
            next: None,
            stopped: None,
            error: None,
            retirement,
            confined: PhantomData,
        };
        if code != 0 || device.handle.is_none() {
            let error = if code == 0 {
                Error::Allocation
            } else {
                native_error(code)
            };
            device.fail(error);
            return Err(error);
        }
        Ok(device)
    }

    pub const fn configuration(&self) -> AudioStreamConfig {
        self.configuration
    }
    pub const fn state(&self) -> State {
        self.state
    }
    pub const fn stop_outcome(&self) -> Option<StopOutcome> {
        self.stopped
    }
    pub fn retirement(&self) -> Retirement {
        self.retirement.clone()
    }
    pub const fn queue_capacity_bytes(&self) -> u32 {
        match self.configuration.channels() {
            fr_core::audio::AudioChannels::Mono => 40 * 48 * 2,
            fr_core::audio::AudioChannels::Stereo => 40 * 48 * 4,
        }
    }

    pub fn poll(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, Error>,
    ) -> Result<State, Error> {
        if self.state == State::Closed {
            return Ok(State::Closed);
        }
        self.operation(|this| {
            let before = checkpoint()?;
            let native = this.read_clock();
            // Permission and elapsed setup time are checked AFTER foreign work.
            let now = checkpoint()?;
            if now.0 < before.0 {
                return Err(Error::Clock);
            }
            match native {
                Ok(reading) => {
                    if this.accept_clock(now, &reading)? {
                        this.state = State::Ready;
                    }
                    this.reading = Some(PlayoutClock {
                        now,
                        output_samples: reading.samples,
                    });
                }
                Err(Error::Pending) => this.clock.pending(now.0)?,
                Err(error) => return Err(error),
            }
            Ok(this.state)
        })
    }

    pub fn clock(&mut self, now: ClientInstant) -> Result<PlayoutClock, Error> {
        self.operation(|this| {
            if this.state != State::Ready {
                return Err(Error::Pending);
            }
            // poll() read the native timeline before its fresh final checkpoint.
            // Keep that actual position; never interpolate elapsed wall time
            // into synthetic consumed samples. This method performs no foreign
            // work after the caller's immediately preceding permission sample.
            let reading = this.reading.ok_or(Error::Pending)?;
            if now.0 < reading.now.0 || now.0 - reading.now.0 >= CLOCK_STALL_US {
                return Err(Error::Clock);
            }
            this.clock.observe(now.0, reading.output_samples)?;
            Ok(PlayoutClock {
                now,
                output_samples: reading.output_samples,
            })
        })
    }

    fn read_clock(&self) -> Result<NativeClock, Error> {
        let handle = self.handle.ok_or(Error::Closed)?;
        let mut clock = NativeClock::default();
        // SAFETY: this worker exclusively owns the handle; C only copies two
        // u64 readings to valid caller storage and retains neither pointer.
        let code = unsafe { fr_macos_audio_clock(handle.as_ptr(), &raw mut clock) };
        if code != 0 {
            return Err(native_error(code));
        }
        Ok(clock)
    }
    fn accept_clock(&mut self, now: ClientInstant, reading: &NativeClock) -> Result<bool, Error> {
        if self.lead.is_some_and(|lead| lead != reading.lead) {
            return Err(Error::DeviceChanged);
        }
        let ready = self.clock.observe(now.0, reading.samples)?;
        self.lead = Some(reading.lead);
        Ok(ready)
    }

    /// Submit one exact, original PCM slot. Native preparation invokes the
    /// final checkpoint immediately before `AudioQueueEnqueueBufferWithParameters`.
    /// A partial/unknown enqueue is never retried on this or a new owner.
    pub fn submit(
        &mut self,
        pcm: &AudioPcmFrame,
        audio: AudioSubmission,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, Error>,
    ) -> Result<Submission, Error> {
        self.operation(|this| {
            if this.state != State::Ready {
                return Err(Error::Pending);
            }
            let samples = validate_pcm(this.configuration, pcm, audio, this.next)?;
            let next = audio.sequence.checked_add(1).ok_or(Error::Metadata)?;
            let window = Window {
                slot_start: audio.output_samples,
                slot_end: audio.output_valid_before,
                valid_until: audio.valid_until.0,
                samples: u64::from(samples),
                lead: this.lead.ok_or(Error::Clock)?,
            };
            let now = checkpoint()?;
            let clock = this.clock(now)?;
            let (start, end) = window.validate(clock.now.0, clock.output_samples)?;
            let mut guard = Permission {
                checkpoint: &mut checkpoint,
                clock: &mut this.clock,
                window,
                error: None,
                observed: None,
            };
            // SAFETY: PCM is a checked, borrowed interleaved i16 slice. C copies
            // it before calling the synchronous nonescaping permission callback;
            // no Rust pointer is retained by Audio Queue. The stack guard remains
            // exclusively borrowed for the entire call and catches any panic.
            let code = unsafe {
                fr_macos_audio_submit(
                    this.handle.ok_or(Error::Closed)?.as_ptr(),
                    pcm.samples().as_ptr(),
                    samples,
                    start,
                    end,
                    permitted,
                    (&raw mut guard).cast(),
                )
            };
            if let Some(error) = guard.error {
                return Err(error);
            }
            if code != 0 {
                return Err(native_error(code));
            }
            this.reading = guard.observed;
            this.next = Some((next, audio.output_valid_before));
            Ok(Submission {
                audio,
                scheduled_output_sample: start,
                native_queue_capacity_bytes: this.queue_capacity_bytes(),
            })
        })
    }

    /// Fence first, then stop/reset only this queue without draining old sound.
    /// The enclosing worker accounts for the actual synchronous native cleanup;
    /// its retirement witness stays false if callback disposal is unconfirmed.
    pub fn begin_stop(&mut self, _now: ClientInstant) -> Result<(), Error> {
        if self.state == State::Closed {
            return Ok(());
        }
        self.state = State::Stopping;
        self.next = None;
        let code = self.handle.map_or(0, |handle| {
            // SAFETY: unique live handle; native stop fences before any API work
            // and never calls the Rust permission callback or keeps a pointer.
            unsafe { fr_macos_audio_stop(handle.as_ptr()) }
        });
        self.disconnect();
        if code != 0 || !self.retirement.is_complete() {
            return Err(Error::Native);
        }
        self.stopped = Some(StopOutcome::Flushed);
        Ok(())
    }

    pub fn disconnect(&mut self) {
        self.state = State::Closed;
        self.next = None;
        self.reading = None;
        self.stopped.get_or_insert(StopOutcome::Disconnected);
        if let Some(handle) = self.handle.take() {
            // SAFETY: C frees only after synchronous callback/queue disposal. On
            // failure it retains bounded state and its native owner slot, so no
            // callback can reach freed memory. Do not retry an ambiguous native
            // disposal: the API does not promise a reusable handle on failure.
            if unsafe { fr_macos_audio_destroy(handle.as_ptr()) } == 0 {
                self.retirement.0.store(true, Ordering::Release);
            }
        }
    }
    fn fail(&mut self, error: Error) {
        self.error.get_or_insert(error);
        self.disconnect();
    }
    fn operation<T>(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let mut guard = Operation {
            device: self,
            completed: false,
        };
        let result = action(guard.device);
        if let Err(error) = result {
            guard.device.fail(error);
        }
        guard.completed = true;
        result
    }
}
impl Drop for PlaybackDevice {
    fn drop(&mut self) {
        self.disconnect();
    }
}
struct Operation<'a> {
    device: &'a mut PlaybackDevice,
    completed: bool,
}
impl Drop for Operation<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.device.fail(Error::Denied);
        }
    }
}

struct Permission<'a> {
    checkpoint: &'a mut dyn FnMut() -> Result<ClientInstant, Error>,
    clock: &'a mut Clock,
    window: Window,
    error: Option<Error>,
    observed: Option<PlayoutClock>,
}
unsafe extern "C" fn permitted(context: *mut c_void, samples: u64) -> c_int {
    // SAFETY: only fr_macos_audio_submit invokes this callback, synchronously,
    // with the same live, exclusive Permission stack object passed above.
    let permission = unsafe { &mut *context.cast::<Permission<'_>>() };
    let result = catch_unwind(AssertUnwindSafe(|| {
        let now = (permission.checkpoint)()?;
        if !permission.clock.observe(now.0, samples)? {
            return Err(Error::Pending);
        }
        permission.window.validate(now.0, samples)?;
        permission.observed = Some(PlayoutClock {
            now,
            output_samples: samples,
        });
        Ok(())
    }));
    match result {
        Ok(Ok(())) => 1,
        Ok(Err(error)) => {
            permission.error = Some(error);
            0
        }
        Err(_) => {
            permission.error = Some(Error::Denied);
            0
        }
    }
}

fn validate_pcm(
    configuration: AudioStreamConfig,
    pcm: &AudioPcmFrame,
    audio: AudioSubmission,
    next: Option<(u64, u64)>,
) -> Result<u32, Error> {
    if audio.direction != AudioDirection::Downlink
        || audio.generation != configuration.generation()
        || pcm.generation() != audio.generation
        || pcm.channels() != configuration.channels()
        || pcm.sample_rate() != 48_000
        || !fr_media::audio::clock::valid_sample_count(
            configuration.expected_samples_per_frame() as usize,
            pcm.samples_per_channel(),
        )
        || pcm.timestamp_samples() != audio.source_samples
        || next.is_some_and(|next| next != (audio.sequence, audio.output_samples))
    {
        return Err(Error::Metadata);
    }
    u32::try_from(pcm.samples_per_channel()).map_err(|_| Error::Metadata)
}

const fn native_error(code: c_int) -> Error {
    match code {
        1 => Error::Pending,
        -1 => Error::Configuration,
        -2 => Error::Unavailable,
        -3 => Error::Allocation,
        -4 => Error::Clock,
        -5 => Error::DeviceChanged,
        -6 => Error::BufferLimit,
        -7 => Error::Backpressure,
        -9 => Error::Denied,
        -10 => Error::SubmissionUnknown,
        -11 => Error::Closed,
        _ => Error::Native,
    }
}
