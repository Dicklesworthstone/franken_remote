use super::{
    AudioDirection, AudioPcmFrame, AudioSubmission, ClientInstant, Error, Guard, LEAD_SAMPLES,
    MAX_DEVICE_BUFFER_MS, PlaybackDevice, PlayoutClock, STOP_US, Stage, StopOutcome, Submission,
    ffi,
};

impl PlaybackDevice {
    /// One bounded, copying, nonblocking write. `checkpoint` rechecks original
    /// downlink/epoch permission at the native boundary. A failed write is terminal
    /// and never retried: effects may already have entered the server connection.
    pub fn submit(
        &mut self,
        pcm: &AudioPcmFrame,
        audio: AudioSubmission,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, Error>,
    ) -> Result<Submission, Error> {
        let mut guard = Guard {
            device: self,
            completed: false,
        };
        let result = guard.device.submit_inner(pcm, audio, &mut checkpoint);
        if let Err(error) = result {
            guard.device.fail(error);
        }
        guard.completed = true;
        result
    }
    fn submit_inner(
        &mut self,
        pcm: &AudioPcmFrame,
        audio: AudioSubmission,
        checkpoint: &mut impl FnMut() -> Result<ClientInstant, Error>,
    ) -> Result<Submission, Error> {
        let clock = self.clock_inner(checkpoint()?)?;
        let samples = u64::from(self.config.expected_samples_per_frame());
        if audio.direction != AudioDirection::Downlink
            || audio.generation != self.config.generation()
            || pcm.generation() != audio.generation
            || pcm.channels() != self.config.channels()
            || pcm.sample_rate() != 48_000
            || u64::try_from(pcm.samples_per_channel()).ok() != Some(samples)
            || pcm.timestamp_samples() != audio.source_samples
            || audio.output_samples.checked_add(samples) != Some(audio.output_valid_before)
            || self
                .next
                .is_some_and(|next| next != (audio.sequence, audio.output_samples))
        {
            return Err(Error::Metadata);
        }
        let next_sequence = audio.sequence.checked_add(1).ok_or(Error::Metadata)?;
        let stream = self.stream()?;
        // SAFETY: copy the public timing value while the nonthreaded loop is idle.
        let timing = unsafe {
            *ffi::pa_stream_get_timing_info(stream)
                .as_ref()
                .ok_or(Error::Clock)?
        };
        // The system output delay is negotiated independently of our packet
        // duration. Choose its lead once, at the first submission, and retain it
        // for the stream. A monitor/device may alter latency during silent setup;
        // never shift already submitted audio or assume the requested target won.
        let lead = match self.lead_samples {
            Some(lead) => lead,
            None => timing
                .configured_sink_usec
                .checked_mul(48)
                .and_then(|n| n.div_ceil(1000).checked_add(5 * 48))
                .filter(|&n| n <= LEAD_SAMPLES)
                .ok_or(Error::BufferLimit)?,
        };
        let target = audio.output_samples.checked_add(lead).ok_or(Error::Clock)?;
        let end = target.checked_add(samples).ok_or(Error::Clock)?;
        validate_submission_clock(clock, audio, end)?;
        let stream = self.stream()?;
        let bytes = std::mem::size_of_val(pcm.samples());
        let offset = target
            .checked_mul(u64::from(self.config.channels().count()) * 2)
            .and_then(|n| i64::try_from(n).ok())
            .ok_or(Error::Clock)?;
        let mut latency = 0_u64;
        let mut negative = 0;
        // SAFETY: owned live native handle and out-pointers; no application callback.
        unsafe {
            let available = ffi::pa_stream_writable_size(stream);
            if available == usize::MAX {
                return Err(Error::Native);
            }
            // writable_size is request credit, NOT capacity. It can be lower
            // than a complete negotiated frame or grow during freewheel silence.
            // Our actual server-read/absolute-end bound below controls capacity.
            if bytes > usize::try_from(self.queue_bytes).map_err(|_| Error::BufferLimit)? {
                return Err(Error::Backpressure);
            }
            if ffi::pa_stream_is_corked(stream) != 0 {
                return Err(Error::Suspended);
            }
            if ffi::pa_stream_get_latency(stream, &raw mut latency, &raw mut negative) < 0 {
                return Err(Error::Clock);
            }
        }
        if negative == 0 && latency > u64::from(MAX_DEVICE_BUFFER_MS) * 1000 {
            return Err(Error::BufferLimit);
        }
        // Last application callback immediately before the native copy/write.
        // No polling, lock, allocation, decode or retry may intervene afterwards.
        let fresh = self.clock_inner(checkpoint()?)?;
        validate_submission_clock(fresh, audio, end)?;
        // Bound the combined server/native/socket sample extent by the capped
        // server queue plus one frame of in-flight allowance. This is a TOTAL
        // bound regardless of which native layer currently retains the bytes.
        // Compare against the last SERVER read position,
        // not only interpolated wall/device time. A stalled server cannot keep
        // accepting a growing native/socket backlog during timing's grace period.
        // SAFETY: public timing value is borrowed only for this immediate copy;
        // no mainloop turn or callback can mutate it concurrently.
        let timing = unsafe {
            *ffi::pa_stream_get_timing_info(stream)
                .as_ref()
                .ok_or(Error::Clock)?
        };
        validate_native_extent(
            timing.read_index,
            timing.read_index_corrupt,
            target,
            end,
            self.config.channels().count(),
            self.queue_bytes
                .checked_add(u32::try_from(bytes).map_err(|_| Error::BufferLimit)?)
                .ok_or(Error::BufferLimit)?,
        )?;
        // SAFETY: exact admitted interleaved i16 shape and real slice byte length.
        // With free_cb=NULL libpulse copies the data synchronously; no Rust pointer
        // is retained. Absolute sample offsets prevent replay/overlap after gaps.
        if unsafe {
            ffi::pa_stream_write(
                stream,
                pcm.samples().as_ptr().cast(),
                bytes,
                None,
                offset,
                ffi::SEEK_ABSOLUTE,
            )
        } < 0
        {
            return Err(Error::SubmissionUnknown);
        }
        self.lead_samples = Some(lead);
        self.next = Some((next_sequence, audio.output_valid_before));
        Ok(Submission {
            audio,
            scheduled_output_sample: target,
            native_queue_capacity_bytes: self.queue_bytes,
        })
    }
    /// Fence immediately, then asynchronously cork and flush this stream only.
    /// `poll` completes it within the ORIGINAL 100 ms stop deadline. No drain of
    /// obsolete sound, server-wide mute or mutation of another application's audio.
    pub fn begin_stop(&mut self, now: ClientInstant) -> Result<(), Error> {
        if matches!(self.stage, Stage::Cork | Stage::Flush | Stage::Closed) {
            return Ok(());
        }
        self.error = Some(Error::Closed);
        self.next = None;
        self.operation = None;
        if self.stream.is_none() || self.stage != Stage::Running {
            self.disconnect();
            return Ok(());
        }
        let Some(deadline) = now
            .0
            .checked_add(STOP_US)
            .filter(|_| now.0 >= self.last_now)
        else {
            self.disconnect();
            return Err(Error::Clock);
        };
        self.deadline = deadline;
        self.last_now = now.0;
        if let Err(error) = self.start_operation(Stage::Cork, now) {
            self.fail(error);
            return Err(error);
        }
        Ok(())
    }
    /// Immediate fallback on revoke, error, cancellation or Drop. Local playback
    /// is permanently fenced and the native stream/context are disconnected. Use
    /// `begin_stop`/`poll` when a server flush acknowledgement is required.
    pub fn disconnect(&mut self) {
        self.stage = Stage::Closed;
        self.next = None;
        self.operation = None;
        self.stopped.get_or_insert(StopOutcome::Disconnected);
        // SAFETY: no callback can run concurrently. Cancel operation userdata first,
        // then disconnect/unref stream, context and finally its event-loop storage.
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
    pub(super) fn fail(&mut self, error: Error) {
        self.error.get_or_insert(error);
        self.disconnect();
    }
}
pub(super) fn validate_submission_clock(
    clock: PlayoutClock,
    audio: AudioSubmission,
    end: u64,
) -> Result<(), Error> {
    if clock.now.0 >= audio.valid_until.0 {
        return Err(Error::Expired);
    }
    if clock.output_samples < audio.output_samples
        || clock.output_samples >= audio.output_valid_before
    {
        return Err(Error::Clock);
    }
    let remaining = end.checked_sub(clock.output_samples).ok_or(Error::Clock)?;
    if remaining > u64::from(MAX_DEVICE_BUFFER_MS) * 48 {
        return Err(Error::BufferLimit);
    }
    let remaining_us = remaining
        .checked_mul(1000)
        .ok_or(Error::Clock)?
        .div_ceil(48);
    if clock
        .now
        .0
        .checked_add(remaining_us)
        .is_none_or(|at| at >= audio.valid_until.0)
    {
        return Err(Error::Expired);
    }
    Ok(())
}

// Absolute stream byte indices, not source timestamps or client write counters.
pub(super) fn validate_native_extent(
    read_index: i64,
    corrupt: i32,
    start_sample: u64,
    end_sample: u64,
    channels: u8,
    queue_bytes: u32,
) -> Result<(), Error> {
    if corrupt != 0 {
        return Err(Error::Clock);
    }
    let read = u64::try_from(read_index).map_err(|_| Error::Clock)?;
    let stride = u64::from(channels) * 2;
    let start = start_sample.checked_mul(stride).ok_or(Error::Clock)?;
    let end = end_sample.checked_mul(stride).ok_or(Error::Clock)?;
    if start < read || end < start {
        return Err(Error::Clock);
    }
    if end - read > u64::from(queue_bytes) {
        return Err(Error::Backpressure);
    }
    Ok(())
}
