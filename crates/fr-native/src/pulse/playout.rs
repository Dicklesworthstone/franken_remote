#![forbid(unsafe_code)]
//! The bound native Opus receiver and selected output share one retiring owner.
//! No public mutable device/decoder escape, reconnect, or implicit audio enable.
use super::{Error as DeviceError, PlaybackDevice, State, StopOutcome, Submission};
use crate::opus::playout::{Error as ReceiveError, OpusPlayout, ReceiveResult};
use fr_client::{
    audio::{
        AudioVolumeControl,
        playout::{PlayoutClock, PlayoutError, PlayoutResult},
    },
    input::ClientInstant,
};
use fr_core::audio::{AudioDirection, AudioStreamConfig};
use fr_wire::audio::{self, AudioConfiguration};
use std::cell::{Cell, RefCell};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Configuration,
    Closed,
    NotAcknowledged,
    AlreadyAcknowledged,
    AcknowledgementUnknown,
    Device(DeviceError),
    Receiver(ReceiveError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "pulse-opus-playout: {self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderResult {
    Waiting,
    Submitted(Submission),
}

/// One admitted downlink channel and an actually configured
/// local output. All clocks come from that device; all PCM reaches its checked
/// native submission. Use only on the supervised audio worker. The containing
/// session owns admission, audio enable/approval and persistent epoch retirement.
/// The checkpoint must recheck that original permission and return fresh LOCAL
/// monotonic time. It is invoked before/after decode and at the OS-facing write.
pub struct PulsePlayout {
    binding: u32,
    receiver: OpusPlayout,
    device: PlaybackDevice,
    error: Option<Error>,
    stopping: bool,
    acknowledged: bool,
    setup_until: ClientInstant,
}
impl PulsePlayout {
    pub fn new(
        binding: u32,
        offer: AudioConfiguration,
        mut device: PlaybackDevice,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, DeviceError>,
    ) -> Result<Self, Error> {
        let config = AudioStreamConfig::new(
            offer.direction,
            offer.generation,
            offer.channels,
            offer.frame_duration_ms,
            offer.jitter_target_ms,
        )
        .map_err(|_| Error::Configuration)?;
        if config.direction() != AudioDirection::Downlink
            || config != device.configuration()
            || device.state() != State::Ready
        {
            return Err(Error::Configuration);
        }
        let clock = device_clock(&mut device, &mut checkpoint).map_err(Error::Device)?;
        let setup_until = ClientInstant(
            clock
                .now
                .0
                .checked_add(super::STARTUP_US)
                .ok_or(Error::Device(DeviceError::Clock))?,
        );
        let receiver = OpusPlayout::new(binding, offer, clock).map_err(Error::Receiver)?;
        // Native codec creation may be slow or permission may change during it.
        // No authority is invented from successful allocation or a ready device.
        device_clock(&mut device, &mut checkpoint).map_err(Error::Device)?;
        Ok(Self {
            binding,
            receiver,
            device,
            error: None,
            stopping: false,
            acknowledged: false,
            setup_until,
        })
    }
    /// Publish `AudioConfigured` only after BOTH the actual device and native
    /// decoder configured successfully. The containing authenticated channel must
    /// accept this exact bounded record before returning Ok; it must not block or
    /// retry partial/unknown sends. A failure or unwind retires both native owners.
    /// This is local send admission, not proof that the remote peer received it.
    pub fn acknowledge(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, DeviceError>,
        send: impl FnOnce(&[u8]) -> Result<(), ()>,
    ) -> Result<(), Error> {
        self.check()?;
        if self.acknowledged {
            return Err(Error::AlreadyAcknowledged);
        }
        self.operation(|this| {
            let clock = device_clock(&mut this.device, &mut checkpoint).map_err(Error::Device)?;
            if clock.now.0 >= this.setup_until.0 {
                return Err(Error::Device(DeviceError::Expired));
            }
            let config = this.device.configuration();
            let mut record = [0; audio::AUDIO_CONFIGURED_RECORD_BYTES];
            audio::encode_configured(
                &audio::AudioConfigured {
                    direction: config.direction(),
                    generation: config.generation(),
                    accepted: true,
                    actual_channels: config.channels(),
                    actual_sample_rate: config.sample_rate(),
                    actual_frame_duration_ms: config.frame_duration_ms(),
                },
                this.binding,
                &mut record,
            )
            .map_err(|e| Error::Receiver(ReceiveError::Wire(e)))?;
            send(&record).map_err(|()| Error::AcknowledgementUnknown)?;
            let fresh = device_clock(&mut this.device, &mut checkpoint).map_err(Error::Device)?;
            if fresh.now.0 >= this.setup_until.0 {
                return Err(Error::Device(DeviceError::Expired));
            }
            this.acknowledged = true;
            Ok(())
        })
    }
    pub const fn configuration(&self) -> AudioConfiguration {
        self.receiver.configuration()
    }
    pub const fn error(&self) -> Option<Error> {
        self.error
    }
    pub const fn state(&self) -> State {
        self.device.state()
    }
    pub const fn stop_outcome(&self) -> Option<StopOutcome> {
        self.device.stop_outcome()
    }
    pub const fn queued_packets(&self) -> usize {
        self.receiver.queued_packets()
    }
    pub const fn device_queue_capacity_bytes(&self) -> u32 {
        self.device.queue_capacity_bytes()
    }
    /// Applies to future submissions. Already submitted device sound is not
    /// silently claimed muted; stop the owner to fence and retire that queue.
    pub fn volume_mut(&mut self) -> &mut AudioVolumeControl {
        self.receiver.volume_mut()
    }
    /// Bounded native progress even while no media arrives. The outer Asupersync
    /// worker supplies readiness/timer scheduling; this method does not spin.
    pub fn service(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, DeviceError>,
    ) -> Result<PlayoutClock, Error> {
        self.check()?;
        self.operation(|this| {
            let clock = device_clock(&mut this.device, &mut checkpoint).map_err(Error::Device)?;
            if !this.acknowledged && clock.now.0 >= this.setup_until.0 {
                return Err(Error::Device(DeviceError::Expired));
            }
            Ok(clock)
        })
    }
    pub fn receive_record(
        &mut self,
        bytes: &[u8],
        mut checkpoint: impl FnMut() -> Result<ClientInstant, DeviceError>,
    ) -> Result<ReceiveResult, Error> {
        self.check()?;
        self.operation(|this| {
            // A valid bound stop fences BEFORE any native mainloop/timing work or
            // application callback. Cleanup cannot be gated by a fresh audio grant.
            if let Ok(stop) = audio::decode_stop(bytes, this.binding) {
                let offer = this.configuration();
                if stop.direction == offer.direction && stop.generation == offer.generation {
                    this.receiver.stop();
                    this.stopping = true;
                    this.error = Some(Error::Closed);
                    let now = checkpoint().map_err(Error::Device)?;
                    this.device.begin_stop(now).map_err(Error::Device)?;
                    return Ok(ReceiveResult::Stopped(stop.reason));
                }
            }
            if !this.acknowledged {
                return Err(Error::NotAcknowledged);
            }
            let clock = device_clock(&mut this.device, &mut checkpoint).map_err(Error::Device)?;
            this.receiver
                .receive_record(bytes, clock)
                .map_err(Error::Receiver)
        })
    }
    pub fn render(
        &mut self,
        mut checkpoint: impl FnMut() -> Result<ClientInstant, DeviceError>,
    ) -> Result<RenderResult, Error> {
        self.check()?;
        if !self.acknowledged {
            return Err(Error::NotAcknowledged);
        }
        self.operation(|this| {
            // Synchronous, nonoverlapping callbacks. RefCell keeps both callback
            // borrows checked without unsafe aliases or opening mutable escapes.
            let device = RefCell::new(&mut this.device);
            let checkpoint = RefCell::new(&mut checkpoint);
            let native_error = Cell::new(None);
            let submitted = Cell::new(None);
            let result = this.receiver.render(
                || {
                    device_clock(&mut device.borrow_mut(), &mut **checkpoint.borrow_mut()).map_err(
                        |error| {
                            native_error.set(Some(error));
                            PlayoutError::Output
                        },
                    )
                },
                |pcm, audio| {
                    device
                        .borrow_mut()
                        .submit(pcm, audio, &mut **checkpoint.borrow_mut())
                        .map(|receipt| submitted.set(Some(receipt)))
                        .map_err(|error| {
                            native_error.set(Some(error));
                            PlayoutError::Output
                        })
                },
            );
            // Keep the actual native refusal, not the generic callback marker.
            if let Some(error) = native_error.get() {
                return Err(Error::Device(error));
            }
            match (result.map_err(Error::Receiver)?, submitted.get()) {
                (PlayoutResult::Waiting, None) => Ok(RenderResult::Waiting),
                (PlayoutResult::Submitted(audio), Some(receipt)) if audio == receipt.audio => {
                    Ok(RenderResult::Submitted(receipt))
                }
                _ => Err(Error::Configuration),
            }
        })
    }
    /// Fences queued codec/packet work first, then begins native cork/flush.
    /// Repeated stop never resets the device's original cleanup deadline.
    pub fn stop(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.receiver.stop();
        self.acknowledged = false;
        self.error.get_or_insert(Error::Closed);
        self.stopping = true;
        self.device.begin_stop(now).map_err(Error::Device)
    }
    /// Cleanup-only polling. Supply fresh local time, not a renewed observation
    /// grant. Never calls the receiver or submits audio. Timeout/error disconnects
    /// and leaves `StopOutcome::Disconnected` rather than inventing a flush ACK.
    pub fn poll_stop(
        &mut self,
        clock: impl FnMut() -> Result<ClientInstant, DeviceError>,
    ) -> Result<State, Error> {
        if !self.stopping {
            return Err(Error::Closed);
        }
        if self.device.state() == State::Closed {
            return Ok(State::Closed);
        }
        self.operation(|this| this.device.poll(clock).map_err(Error::Device))
    }
    /// Cancellation/revocation fallback; permanently retire both owners even if
    /// the containing task keeps this value alive after an error or caught panic.
    pub fn disconnect(&mut self) {
        self.receiver.stop();
        self.acknowledged = false;
        self.error.get_or_insert(Error::Closed);
        self.stopping = true;
        self.device.disconnect();
    }
    fn check(&self) -> Result<(), Error> {
        self.error.map_or(Ok(()), Err)
    }
    fn operation<T>(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let mut guard = Guard {
            owner: self,
            completed: false,
        };
        let result = action(guard.owner);
        if let Err(error) = result {
            guard.owner.error.get_or_insert(error);
            guard.owner.disconnect();
        }
        guard.completed = true;
        result
    }
}
impl Drop for PulsePlayout {
    fn drop(&mut self) {
        self.disconnect();
    }
}
struct Guard<'a> {
    owner: &'a mut PulsePlayout,
    completed: bool,
}
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.owner.disconnect();
        }
    }
}
fn device_clock(
    device: &mut PlaybackDevice,
    checkpoint: &mut impl FnMut() -> Result<ClientInstant, DeviceError>,
) -> Result<PlayoutClock, DeviceError> {
    device.poll(&mut *checkpoint)?;
    device.clock(checkpoint()?)
}
