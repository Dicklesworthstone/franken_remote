//! Bounded real-graph demand for an asynchronous, supervised decoder owner.
//!
//! `Ready` only makes a source selectable. Demand exists only after a consumer
//! starts its graph. Tokens name one original graph interval and endpoint; a
//! later callback cannot refresh a pending decoder operation's reservation.
use super::{Error, MAX_BLOCK_FRAMES, MAX_RETAINED_AGE, Microphone, Native};
use fr_core::audio::AudioGeneration;
use fr_media::audio::{AudioPcmFrame, clock::valid_sample_count};
use std::{
    ffi::c_int,
    time::{Duration, Instant},
};

/// One observed request from the original running graph. `graph_samples` is
/// its real consumed position; the output interval is separate scheduled work,
/// not a claim that an application has recorded or observed the sound.
#[derive(Debug, Clone, Copy)]
pub struct Demand {
    native: NativeDemand,
    identity: u64,
    generation: AudioGeneration,
    until: Instant,
}
impl Demand {
    pub const fn graph_samples(self) -> u64 {
        self.native.graph_samples
    }
    pub const fn output_start(self) -> u64 {
        self.native.output_start
    }
    pub const fn output_end(self) -> u64 {
        self.native.output_end
    }
    pub const fn available_blocks(self) -> usize {
        self.native.available_blocks as usize
    }
    pub const fn valid_until(self) -> Instant {
        self.until
    }
    pub const fn serial(self) -> u64 {
        self.native.serial
    }
    pub const fn generation(self) -> AudioGeneration {
        self.generation
    }
    pub const fn frames(self) -> u32 {
        self.native.frames
    }
    /// One callback for supervised preparation and one asynchronous native
    /// handoff. This scheduled lead is never added to the consumed clock.
    pub const fn lead_frames(self) -> u32 {
        self.native.frames * 2
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct NativeDemand {
    serial: u64,
    graph_samples: u64,
    output_start: u64,
    output_end: u64,
    until: u64,
    available_blocks: u32,
    frames: u32,
}
unsafe extern "C" {
    fn fr_pipewire_mic_enable_demand(mic: *mut Native) -> c_int;
    fn fr_pipewire_mic_demand(mic: *mut Native, out: *mut NativeDemand) -> c_int;
    fn fr_pipewire_mic_submit_demand(
        mic: *mut Native,
        pcm: *const i16,
        frames: u32,
        serial: u64,
        start: u64,
        until: u64,
    ) -> c_int;
}

impl Microphone {
    /// Select paced mode and service one bounded native turn. Before a real
    /// consumer starts the graph this returns `None`, without an invented clock
    /// or a PCM queue. Supported graph requests contain at most 1024 frames;
    /// larger requests cannot meet this endpoint's four-block latency budget.
    /// A running graph's pause, missed interval, expiry, quantum/rate change or
    /// discontinuity retires the original endpoint. Raw `submit` cannot be mixed
    /// with this mode. Repeated reads retain the same token's original deadline.
    pub fn poll_demand(
        &mut self,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<Option<Demand>, Error> {
        self.check_permission(&mut permitted)?;
        let result = self.poll_demand_inner(&mut permitted);
        if result.is_err() {
            self.close();
        }
        result
    }
    fn poll_demand_inner(
        &mut self,
        permitted: &mut impl FnMut() -> bool,
    ) -> Result<Option<Demand>, Error> {
        let native = self.native.ok_or(Error::Closed)?;
        if !self.demand_mode {
            if self.last_end.is_some() {
                return Err(Error::InvalidConfiguration);
            }
            // SAFETY: the unique context is idle on its owner thread. No raw
            // PCM has been admitted, and this operation installs no callback.
            if unsafe { fr_pipewire_mic_enable_demand(native.as_ptr()) } != 0 {
                return Err(Error::InvalidConfiguration);
            }
            self.demand_mode = true;
        }
        self.poll(&mut *permitted)?;
        self.check_permission(permitted)?;
        let mut snapshot = NativeDemand::default();
        // SAFETY: same thread's live context and one writable fixed ABI value.
        // C reads a sequence-protected PipeWire timing snapshot, not a Rust
        // pointer or a submitted-frame counter.
        let result = unsafe { fr_pipewire_mic_demand(native.as_ptr(), &raw mut snapshot) };
        self.check_permission(permitted)?;
        match result {
            0 => Ok(None),
            1 => {
                let until = self
                    .control
                    .0
                    .origin
                    .checked_add(Duration::from_micros(snapshot.until))
                    .ok_or(Error::Timeline)?;
                if Instant::now() >= until {
                    return Err(Error::Expired);
                }
                Ok(Some(Demand {
                    native: snapshot,
                    identity: self.identity,
                    generation: self.config.generation(),
                    until,
                }))
            }
            _ => Err(Error::Timeline),
        }
    }

    /// Admit one real, possibly drift-adjusted decoded frame into the fixed C
    /// queue. `start` is the frame's absolute scheduled graph sample; source
    /// timestamps advance by the nominal codec duration independently of that
    /// adjusted span. A frame may cross the demand's end: its exact remainder
    /// stays bounded and keeps its original packet expiry and sample interval.
    /// The native boundary independently checks the token, capacity, permission,
    /// wall expiry and graph interval immediately before publication.
    pub fn submit_for_demand(
        &mut self,
        frame: &AudioPcmFrame,
        demand: Demand,
        start: u64,
        expires: Instant,
        mut permitted: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        self.check_permission(&mut permitted)?;
        let result = self.submit_demand_inner(frame, demand, start, expires, &mut permitted);
        if result.is_err() {
            self.close();
        }
        result
    }
    fn submit_demand_inner(
        &mut self,
        frame: &AudioPcmFrame,
        demand: Demand,
        start: u64,
        expires: Instant,
        permitted: &mut impl FnMut() -> bool,
    ) -> Result<(), Error> {
        if !self.demand_mode
            || demand.identity != self.identity
            || demand.generation != self.config.generation()
        {
            return Err(Error::Generation);
        }
        let next = validate_frame(self.config, frame, self.demand_source_end)?;
        if start < demand.output_start() || start >= demand.output_end() {
            return Err(Error::Timeline);
        }
        let now = Instant::now();
        if now >= expires || now >= demand.until {
            return Err(Error::Expired);
        }
        if expires > now.checked_add(MAX_RETAINED_AGE).ok_or(Error::Expired)? {
            return Err(Error::InvalidConfiguration);
        }
        // Token validity bounds admission of this exact operation. Once
        // accepted, a frame's carried tail retains its ORIGINAL packet expiry
        // and absolute sample interval; a callback token is not a new PCM age.
        let until = expires
            .checked_duration_since(self.control.0.origin)
            .and_then(|d| u64::try_from(d.as_micros()).ok())
            .ok_or(Error::Expired)?;
        self.check_permission(permitted)?;
        let native = self.native.ok_or(Error::Closed)?;
        self.demand_source_end = Some(next);
        // SAFETY: exact checked channels, initialized PCM and <=961 frames.
        // C copies bounded content and metadata; it retains no Rust borrow.
        match unsafe {
            fr_pipewire_mic_submit_demand(
                native.as_ptr(),
                frame.samples().as_ptr(),
                u32::try_from(frame.samples_per_channel()).map_err(|_| Error::Format)?,
                demand.serial(),
                start,
                until,
            )
        } {
            0 => Ok(()),
            1 => Err(Error::Backpressure),
            2 => Err(Error::Expired),
            3 => Err(Error::PermissionEnded),
            _ => Err(Error::Timeline),
        }
    }
}

fn validate_frame(
    config: fr_core::audio::AudioStreamConfig,
    frame: &AudioPcmFrame,
    source_end: Option<u64>,
) -> Result<u64, Error> {
    if frame.generation() != config.generation() {
        return Err(Error::Generation);
    }
    if frame.channels() != config.channels()
        || frame.sample_rate() != 48_000
        || frame.samples_per_channel() > MAX_BLOCK_FRAMES
        || !valid_sample_count(
            config.expected_samples_per_frame() as usize,
            frame.samples_per_channel(),
        )
    {
        return Err(Error::Format);
    }
    if source_end.is_some_and(|end| frame.timestamp_samples() != end) {
        return Err(Error::Timeline);
    }
    frame
        .timestamp_samples()
        .checked_add(u64::from(config.expected_samples_per_frame()))
        .ok_or(Error::Timeline)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::audio::{AudioChannels, AudioDirection, AudioStreamConfig};
    #[test]
    fn graph_rounding_never_changes_the_nominal_source_timeline() {
        for nominal in [480_usize, 960] {
            let config = AudioStreamConfig::new(
                AudioDirection::Uplink,
                AudioGeneration::from_raw(7),
                AudioChannels::Stereo,
                u16::try_from(nominal / 48).unwrap(),
                20,
            )
            .unwrap();
            for actual in [nominal - 1, nominal, nominal + 1] {
                let frame = AudioPcmFrame::from_interleaved(
                    config.generation(),
                    config.channels(),
                    2000,
                    &vec![1; actual * 2],
                )
                .unwrap();
                assert_eq!(
                    validate_frame(config, &frame, Some(2000)),
                    Ok(2000 + nominal as u64)
                );
                assert_eq!(
                    validate_frame(config, &frame, Some(1999)),
                    Err(Error::Timeline)
                );
            }
        }
    }
}
