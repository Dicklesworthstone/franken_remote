use super::{
    Cell, ClientInstant, Error, Guard, MAX_DEVICE_BUFFER_MS, NonNull, OUTPUT_LEAD_MS, Operation,
    PlaybackDevice, PlayoutClock, Stage, State, StopOutcome, TIMING_AGE_US, TIMING_PERIOD_US,
    c_void, ffi, success,
};

impl PlaybackDevice {
    /// At most four nonblocking native turns, one stream and one operation.
    /// No sleep, worker creation or unbounded catch-up loop. Use the containing
    /// Asupersync worker's timer/readiness to call again; this function is not a
    /// busy-polling service loop. `checkpoint` must service the actual local grant.
    pub fn poll(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, Error>,
    ) -> Result<State, Error> {
        let mut guard = Guard {
            device: self,
            completed: false,
        };
        let result = guard.device.poll_inner(&mut checkpoint);
        if let Err(error) = result {
            guard.device.fail(error);
        }
        guard.completed = true;
        result
    }
    fn poll_inner(
        &mut self,
        checkpoint: &mut impl FnMut() -> Result<ClientInstant, Error>,
    ) -> Result<State, Error> {
        for _ in 0..4 {
            let now = checkpoint()?;
            self.advance_time(now)?;
            let mainloop = self.mainloop.ok_or(Error::Closed)?;
            // SAFETY: live, exclusive native loop; block=0 is explicitly polled
            // mode. Only our bounded operation-completion callbacks are installed.
            if unsafe { ffi::pa_mainloop_iterate(mainloop.as_ptr(), 0, std::ptr::null_mut()) } < 0 {
                return Err(Error::Native);
            }
            self.progress(checkpoint()?)?;
            if matches!(self.stage, Stage::Running | Stage::Closed) {
                break;
            }
        }
        Ok(self.state())
    }
    fn progress(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.advance_time(now)?;
        let context = self.context.ok_or(Error::Closed)?;
        // SAFETY: owned handles, read-only state queries.
        let context_state = unsafe { ffi::pa_context_get_state(context.as_ptr()) };
        if context_state > ffi::CONTEXT_READY {
            return Err(Error::Unavailable);
        }
        if self.stage == Stage::Context {
            if context_state == ffi::CONTEXT_READY {
                self.connect_stream()?;
            }
            return Ok(());
        }
        let stream = self.stream()?;
        let stream_state = unsafe { ffi::pa_stream_get_state(stream) };
        if stream_state > ffi::STREAM_READY {
            return Err(Error::DeviceChanged);
        }
        if self.stage == Stage::Stream {
            if stream_state == ffi::STREAM_READY {
                self.validate_device()?;
                self.start_operation(Stage::Uncork, now)?;
            }
            return Ok(());
        }
        self.validate_device()?;
        if let Some(op) = &self.operation {
            if now.0.saturating_sub(op.requested_at) >= TIMING_AGE_US
                && self.stage == Stage::Running
            {
                return Err(Error::Clock);
            }
            if op.result()?.is_some() {
                let requested_at = op.requested_at;
                self.operation = None;
                match self.stage {
                    Stage::Uncork => self.start_operation(Stage::Timing, now)?,
                    Stage::Timing | Stage::Running => {
                        if now.0.saturating_sub(requested_at) >= TIMING_AGE_US {
                            return Err(Error::Clock);
                        }
                        self.timing_at = Some(requested_at);
                        // Configuration acceptance is not device-clock readiness. An
                        // empty cold sink can have seconds of already queued silence
                        // while get_latency() misleadingly clamps its result to zero.
                        // Keep waiting under the original startup deadline until the
                        // actual sink delay and advancing playback clock qualify.
                        if self.stage == Stage::Running || self.clock_qualified(now)? {
                            self.stage = Stage::Running;
                        }
                    }
                    Stage::Cork => self.start_operation(Stage::Flush, now)?,
                    Stage::Flush => {
                        self.disconnect();
                        self.stopped = Some(StopOutcome::Flushed);
                    }
                    _ => return Err(Error::Native),
                }
            }
        }
        if matches!(self.stage, Stage::Running | Stage::Timing)
            && self.operation.is_none()
            && self
                .timing_at
                .is_some_and(|at| now.0.saturating_sub(at) >= TIMING_PERIOD_US)
        {
            self.start_operation(self.stage, now)?;
        }
        Ok(())
    }
    fn connect_stream(&mut self) -> Result<(), Error> {
        let spec = ffi::SampleSpec {
            format: ffi::S16_NATIVE,
            rate: 48_000,
            channels: self.config.channels().count(),
        };
        let stride = u32::from(spec.channels) * 2;
        let attr = ffi::BufferAttr {
            maxlength: u32::from(MAX_DEVICE_BUFFER_MS) * 48 * stride,
            // Includes sink latency AND stream request credit. A 20 ms total
            // target only requests 15 ms: less than one complete 20 ms frame.
            // Keep the same hard 40 ms ceiling; never wait on a partial write.
            tlength: u32::from(MAX_DEVICE_BUFFER_MS) * 48 * stride,
            prebuf: 0,
            minreq: 5 * 48 * stride,
            fragsize: u32::MAX,
        };
        // SAFETY: valid public C layouts and borrowed NUL-terminated names, no
        // format-fix flags. C copies these arguments before returning.
        unsafe {
            self.stream = Some(
                NonNull::new(ffi::pa_stream_new(
                    self.context.ok_or(Error::Closed)?.as_ptr(),
                    c"Remote playback".as_ptr(),
                    &raw const spec,
                    std::ptr::null(),
                ))
                .ok_or(Error::Allocation)?,
            );
            if ffi::pa_stream_connect_playback(
                self.stream()?,
                self.selection.sink.as_ptr(),
                &raw const attr,
                ffi::START_CORKED
                    | ffi::INTERPOLATE_TIMING
                    | ffi::DONT_MOVE
                    | ffi::ADJUST_LATENCY
                    | ffi::FAIL_ON_SUSPEND,
                std::ptr::null(),
                std::ptr::null_mut(),
            ) < 0
            {
                return Err(Error::Unavailable);
            }
        }
        self.stage = Stage::Stream;
        Ok(())
    }
    fn validate_device(&mut self) -> Result<(), Error> {
        let stream = self.stream()?;
        // SAFETY: C-returned value pointers remain owned by the live stream. Copy
        // immediately; no pointer/reference escapes or survives a mainloop turn.
        let (spec, attr, index, suspended) = unsafe {
            let spec = ffi::pa_stream_get_sample_spec(stream)
                .as_ref()
                .ok_or(Error::Native)?;
            let attr = ffi::pa_stream_get_buffer_attr(stream)
                .as_ref()
                .ok_or(Error::Native)?;
            (
                *spec,
                *attr,
                ffi::pa_stream_get_device_index(stream),
                ffi::pa_stream_is_suspended(stream),
            )
        };
        if spec.format != ffi::S16_NATIVE
            || spec.rate != 48_000
            || spec.channels != self.config.channels().count()
        {
            return Err(Error::Configuration);
        }
        let stride = u32::from(spec.channels) * 2;
        if attr.maxlength == 0
            || attr.maxlength > u32::from(MAX_DEVICE_BUFFER_MS) * 48 * stride
            || attr.tlength == 0
            || attr.tlength > attr.maxlength
            || attr.prebuf != 0
            || attr.minreq == 0
            || attr.minreq > attr.maxlength
            || attr.maxlength % stride != 0
            || self.queue_bytes != 0 && self.queue_bytes != attr.maxlength
        {
            return Err(Error::BufferLimit);
        }
        if suspended != 0 {
            return Err(Error::Suspended);
        }
        if index == u32::MAX || self.device_index.is_some_and(|old| old != index) {
            return Err(Error::DeviceChanged);
        }
        self.device_index = Some(index);
        self.queue_bytes = attr.maxlength;
        Ok(())
    }
    pub(super) fn start_operation(
        &mut self,
        stage: Stage,
        now: ClientInstant,
    ) -> Result<(), Error> {
        if self.operation.is_some() {
            return Err(Error::Native);
        }
        let stream = self.stream()?;
        let reply = Box::new(Cell::new(0));
        let data = std::ptr::from_ref(reply.as_ref())
            .cast_mut()
            .cast::<c_void>();
        // SAFETY: userdata stays in the Operation until cancellation/unref. Only
        // this nonthreaded mainloop dispatches callbacks; no Rust borrow is held.
        let ptr = unsafe {
            match stage {
                Stage::Uncork => ffi::pa_stream_cork(stream, 0, Some(success), data),
                Stage::Cork => ffi::pa_stream_cork(stream, 1, Some(success), data),
                Stage::Flush => ffi::pa_stream_flush(stream, Some(success), data),
                Stage::Timing | Stage::Running => {
                    ffi::pa_stream_update_timing_info(stream, Some(success), data)
                }
                _ => return Err(Error::Native),
            }
        };
        self.operation = Some(Operation {
            ptr: NonNull::new(ptr).ok_or(Error::Native)?,
            reply,
            requested_at: now.0,
        });
        self.stage = stage;
        Ok(())
    }
    /// A fresh client time plus libpulse's interpolated device playback position,
    /// NOT host time or bytes submitted. Timing responses older than 50 ms (age
    /// starts at request, not delayed response) refuse. Native timing is an
    /// estimate, not an instrumented speaker clock; hardware qualification is separate.
    pub fn clock(&mut self, now: ClientInstant) -> Result<PlayoutClock, Error> {
        let result = self.clock_inner(now);
        if let Err(error) = result
            && error != Error::Pending
        {
            self.fail(error);
        }
        result
    }
    pub(super) fn clock_inner(&mut self, now: ClientInstant) -> Result<PlayoutClock, Error> {
        self.advance_time(now)?;
        if self.stage != Stage::Running {
            return Err(Error::Pending);
        }
        self.validate_device()?;
        if self
            .timing_at
            .is_none_or(|at| now.0.saturating_sub(at) >= TIMING_AGE_US)
        {
            return Err(Error::Clock);
        }
        let samples = self.native_clock()?.ok_or(Error::Clock)?;
        if samples < self.last_samples {
            return Err(Error::Clock);
        }
        self.last_samples = samples;
        Ok(PlayoutClock {
            now,
            output_samples: samples,
        })
    }
    // A positive first interpolated timestamp is not proof of stable pacing.
    // The native smoother can accelerate during startup. Require 40 ms of
    // observed progression within a 2 ms phase envelope before admitting audio;
    // resets remain inside the original, never-renewed startup deadline.
    fn clock_qualified(&mut self, now: ClientInstant) -> Result<bool, Error> {
        let Some(samples) = self.native_clock()? else {
            self.clock_probe = None;
            return Ok(false);
        };
        let Some((at, first)) = self.clock_probe else {
            self.clock_probe = Some((now.0, samples));
            return Ok(false);
        };
        let elapsed = now.0.checked_sub(at).ok_or(Error::Clock)?;
        let elapsed_samples = elapsed.checked_mul(48).ok_or(Error::Clock)? / 1000;
        let Some(progress) = samples.checked_sub(first) else {
            return Err(Error::Clock);
        };
        if progress.abs_diff(elapsed_samples) > 96 {
            self.clock_probe = Some((now.0, samples));
            return Ok(false);
        }
        Ok(elapsed >= 40_000)
    }
    fn native_clock(&self) -> Result<Option<u64>, Error> {
        let stream = self.stream()?;
        let mut time = 0_u64;
        // SAFETY: public native value layout; copy before any mainloop turn.
        // get_time uses the same timing owner. The local library/server is trusted
        // for this clock estimate, not treated as physical audibility evidence.
        let timing = unsafe {
            let timing = *ffi::pa_stream_get_timing_info(stream)
                .as_ref()
                .ok_or(Error::Clock)?;
            if timing.read_index_corrupt != 0 || ffi::pa_stream_get_time(stream, &raw mut time) < 0
            {
                return Err(Error::Clock);
            }
            timing
        };
        if timing.playing == 0
            || time == 0
            || timing.configured_sink_usec > u64::from(OUTPUT_LEAD_MS) * 1000
            || timing
                .sink_usec
                .checked_add(timing.transport_usec)
                .is_none_or(|delay| delay > u64::from(OUTPUT_LEAD_MS) * 1000)
        {
            return Ok(None);
        }
        Ok(Some(time.checked_mul(48).ok_or(Error::Clock)? / 1000))
    }
}
