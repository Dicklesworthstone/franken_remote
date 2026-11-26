//! Thread-confined WASAPI playback for one explicitly requested downlink.
//! The default endpoint is pinned once. The native speaker clock, original
//! sample slots and wall deadlines survive every buffer copy and driver call.
//! The separately supervised Opus process owns all hostile media decoding.
use crate::macos_audio_policy::{CLOCK_STALL_US, Clock, Window};
pub use crate::macos_audio_policy::{Error, STARTUP_US};
use fr_client::{
    audio::playout::{AudioSubmission, PlayoutClock},
    input::ClientInstant,
};
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_media::audio::AudioPcmFrame;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

mod native;

/// Suspend-inclusive local time for the playback proxy and its worker.
#[cfg(feature = "windows-audio")]
pub(crate) fn playback_now_us() -> u64 {
    // SAFETY: an infallible kernel counter read with no pointer arguments.
    let ticks = unsafe { windows::Win32::System::WindowsProgramming::QueryInterruptTimePrecise() };
    ticks / 10 // 100 ns ticks to microseconds
}

/// Confirmed retirement of this original output. An uncertain native stop or
/// reset keeps the witness false; dropping a Rust value cannot forge success.
#[derive(Clone)]
pub struct Retirement(Arc<AtomicBool>);
impl Retirement {
    pub fn is_complete(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub struct Selection(());
impl Selection {
    #[allow(clippy::unnecessary_wraps)] // same explicit local selection contract as other outputs
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
    Flushed,
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Submission {
    pub audio: AudioSubmission,
    pub scheduled_output_sample: u64,
    pub native_queue_capacity_bytes: u32,
}

/// Unique, !Send/!Sync native owner. Construct and retire this value inside
/// the existing bounded audio output worker, never on the authority thread.
pub struct PlaybackDevice {
    native: Option<native::Render>,
    configuration: AudioStreamConfig,
    state: State,
    clock: Clock,
    reading: Option<PlayoutClock>,
    lead: u64,
    capacity_bytes: u32,
    next: Option<(u64, u64)>,
    stopped: Option<StopOutcome>,
    error: Option<Error>,
    retirement: Retirement,
}
impl std::fmt::Debug for PlaybackDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WindowsAudioPlaybackDevice")
            .field("state", &self.state)
            .field("error", &self.error)
            .field("retired", &self.retirement.is_complete())
            .finish_non_exhaustive()
    }
}
impl PlaybackDevice {
    pub fn connect(
        selection: Selection,
        configuration: AudioStreamConfig,
        now: ClientInstant,
    ) -> Result<Self, Error> {
        Self::connect_retained(selection, configuration, now).0
    }

    /// The witness also survives partial setup. No successful allocation or
    /// Start return is readiness; poll must observe actual native clock motion.
    pub fn connect_retained(
        _selection: Selection,
        configuration: AudioStreamConfig,
        now: ClientInstant,
    ) -> (Result<Self, Error>, Retirement) {
        let retirement = Retirement(Arc::new(AtomicBool::new(true)));
        let result = Self::create(configuration, now, retirement.clone());
        (result, retirement)
    }

    fn create(
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
        let output = native::Render::open(
            configuration.channels().count(),
            largest,
            retirement.clone(),
        )?;
        Ok(Self {
            lead: output.lead(),
            capacity_bytes: output.capacity_bytes(),
            native: Some(output),
            configuration,
            state: State::Connecting,
            clock: Clock::new(now.0),
            reading: None,
            next: None,
            stopped: None,
            error: None,
            retirement,
        })
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
        self.capacity_bytes
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
            let native = this
                .native
                .as_mut()
                .ok_or(Error::Closed)?
                .poll(|| checkpoint().map(drop));
            let now = checkpoint()?;
            if now.0 < before.0 {
                return Err(Error::Clock);
            }
            match native {
                Ok(reading) => {
                    if this.clock.observe(now.0, reading.samples)? {
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

    /// Return the latest real device sample position. Local elapsed time is
    /// never converted into invented consumed audio samples.
    pub fn clock(&mut self, now: ClientInstant) -> Result<PlayoutClock, Error> {
        self.operation(|this| {
            if this.state != State::Ready {
                return Err(Error::Pending);
            }
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

    /// Write one original slot, with an explicit bounded silence gap if needed.
    /// A native lease is released with real PCM only after a fresh native clock
    /// read and the final original permission, sample-slot and wall checks.
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
            let frames = validate_pcm(this.configuration, pcm, audio, this.next)?;
            let next = audio.sequence.checked_add(1).ok_or(Error::Metadata)?;
            let window = Window {
                slot_start: audio.output_samples,
                slot_end: audio.output_valid_before,
                valid_until: audio.valid_until.0,
                samples: u64::from(frames),
                lead: this.lead,
            };
            let now = checkpoint()?;
            let clock = this.clock(now)?;
            let (start, _) = window.validate(clock.now.0, clock.output_samples)?;
            let mut observed = None;
            let output = this.native.as_mut().ok_or(Error::Closed)?;
            let progress = &mut this.clock;
            output.submit(pcm.samples(), frames, window, |reading| {
                let now = checkpoint()?;
                if !progress.observe(now.0, reading.samples)? {
                    return Err(Error::Pending);
                }
                window.validate(now.0, reading.samples)?;
                observed = Some(PlayoutClock {
                    now,
                    output_samples: reading.samples,
                });
                Ok(())
            })?;
            this.reading = observed;
            this.next = Some((next, audio.output_valid_before));
            Ok(Submission {
                audio,
                scheduled_output_sample: start,
                native_queue_capacity_bytes: this.capacity_bytes,
            })
        })
    }

    /// Fence first, then stop/reset this stream and release its COM interfaces.
    /// Already delivered samples cannot be retracted; this is not audibility.
    pub fn begin_stop(&mut self, _now: ClientInstant) -> Result<(), Error> {
        if self.state == State::Closed {
            return Ok(());
        }
        self.state = State::Stopping;
        let result = self.retire_native();
        if result.is_ok() && self.retirement.is_complete() {
            self.stopped = Some(StopOutcome::Flushed);
            Ok(())
        } else {
            Err(Error::Native)
        }
    }

    pub fn disconnect(&mut self) {
        let _ = self.retire_native();
    }

    fn retire_native(&mut self) -> Result<(), Error> {
        self.state = State::Closed;
        self.next = None;
        self.reading = None;
        self.stopped.get_or_insert(StopOutcome::Disconnected);
        self.native
            .take()
            .map_or(Ok(()), |mut output| output.close())
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
