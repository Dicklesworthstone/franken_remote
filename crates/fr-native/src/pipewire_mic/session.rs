#![forbid(unsafe_code)]
//! Real virtual-microphone output bound to the original host controller.
//!
//! This owner runs on a native worker. The containing authenticated session
//! supplies its completed `AudioUp`
//! channel, independent local microphone approval and original host clock. The
//! original `InputSession` remains owned by its input worker. No numeric wire IDs
//! create or renew authority here, and a microphone-only stop never revokes input.
//!
//! Construct, service and drop `Receiver` on its original native worker thread.
//! `PipeWire` polling/destruction stays outside authority/network callbacks.
//! `Control::stop` fences pending native output and requests codec retirement
//! without running foreign code; actual endpoint destruction requires the worker
//! to service/close, and actual child reaping remains separately observable.
//! This slice does not add a process supervisor for the `PipeWire` endpoint,
//! a network listener, approval UI or public microphone capability.
use super::{
    Microphone, State, Statistics,
    uplink::{self, ReceiveResult, RenderResult, Uplink},
};
use crate::opus::process::Retirement;
use fr_core::{
    audio::{AudioDirection, AudioStreamConfig, MAX_OPUS_PAYLOAD_BYTES},
    ids::{InputLeaseId, InputTicketId},
    input_submission::{InputMonitor, InputSession, Refusal},
    time::HostInstant,
};
use fr_wire::{
    WireError,
    audio::{self, AudioConfiguration},
    negotiation::{ControlBinding, Selection},
};
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotSelected,
    Permission,
    Binding,
    Configuration,
    Clock,
    Stopped,
    AlreadyAttached,
    Authority(Refusal),
    Wire(WireError),
    Device(super::Error),
    Uplink(uplink::Error),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "host-microphone-session: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Metadata of the completed original authenticated `AudioUp` attachment. The
/// transport coordinator must prove route ownership; copied numbers alone are
/// not such proof. The packet ceiling includes the complete FRD0 record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Channel {
    pub parent: ControlBinding,
    pub id: u32,
    pub packet_maximum: usize,
}
#[derive(Clone, Copy)]
struct Clock {
    origin: Instant,
    host: HostInstant,
}
impl Clock {
    fn bind(sample: impl FnOnce() -> HostInstant) -> Result<Self, Error> {
        // Before sampling the real authority clock: extrapolation is equal or
        // conservatively ahead, never a relative TTL starting at worker dequeue.
        let origin = Instant::now();
        let host = catch_unwind(AssertUnwindSafe(sample)).map_err(|_| Error::Clock)?;
        Ok(Self { origin, host })
    }
    fn now(self) -> Result<HostInstant, Error> {
        let elapsed = Instant::now()
            .checked_duration_since(self.origin)
            .ok_or(Error::Clock)?;
        let micros = u64::try_from(elapsed.as_nanos().div_ceil(1000)).map_err(|_| Error::Clock)?;
        self.host
            .as_micros()
            .checked_add(micros)
            .map(HostInstant::from_micros)
            .ok_or(Error::Clock)
    }
}
#[derive(Default)]
struct Native {
    device: Option<super::Control>,
    decoder: Option<Retirement>,
}
struct Shared {
    channel: Channel,
    lease: InputLeaseId,
    monitor: InputMonitor,
    clock: Clock,
    stopped: AtomicBool,
    native: Mutex<Native>,
}
impl Shared {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let (device, decoder) = {
            let native = self
                .native
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (native.device.clone(), native.decoder.clone())
        };
        // Native gates are atomics; codec stop only clears its bounded mailbox
        // and wakes its existing supervisor. No foreign call or input revoke.
        if let Some(device) = device {
            device.stop();
        }
        if let Some(decoder) = decoder {
            decoder.stop();
        }
    }
    fn check(&self) -> Result<HostInstant, Error> {
        if self.stopped.load(Ordering::Acquire) {
            return Err(Error::Stopped);
        }
        let checked = self.clock.now().and_then(|now| {
            self.monitor.deadline(now).map_err(Error::Authority)?;
            if self.stopped.load(Ordering::Acquire) {
                return Err(Error::Stopped);
            }
            Ok(now)
        });
        if checked.is_err() {
            self.stop();
        }
        checked
    }
    fn bind_device(&self, device: super::Control) -> Result<(), Error> {
        let mut native = self.native.lock().map_err(|_| Error::Stopped)?;
        if native.device.is_some() {
            return Err(Error::AlreadyAttached);
        }
        native.device = Some(device);
        drop(native);
        self.check().map(|_| ())
    }
    fn bind_decoder(&self, decoder: Retirement) -> Result<(), Error> {
        let mut native = self.native.lock().map_err(|_| Error::Stopped)?;
        if native.decoder.is_some() {
            return Err(Error::AlreadyAttached);
        }
        native.decoder = Some(decoder);
        drop(native);
        self.check().map(|_| ())
    }
}

/// Mic-only local consent/stop capability for the original owner. A clone
/// retains policy metadata, never a device or live input owner. It cannot enable
/// a stopped microphone or authorize a replacement session with equal IDs.
#[derive(Clone)]
pub struct Control(Arc<Shared>);
impl std::fmt::Debug for Control {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMicrophoneControl")
            .field("stopped", &self.0.stopped.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}
impl Control {
    pub fn stop(&self) {
        self.0.stop();
    }
    /// Fresh original-controller check for a containing native supervisor. This
    /// may fence the microphone, but never creates or renews controller authority.
    pub fn permitted(&self) -> bool {
        self.0.check().is_ok()
    }
    pub fn is_stopped(&self) -> bool {
        !self.permitted()
    }
    pub fn decoder_retirement(&self) -> Option<Retirement> {
        self.0
            .native
            .lock()
            .ok()
            .and_then(|native| native.decoder.clone())
    }
}

/// One independently approved microphone handoff minted from a real native
/// input owner before that owner moves to its worker. Not Clone or reconstructible
/// from `InputMonitor`/lease numbers. Destruction fences all escaped controls.
pub struct Authority {
    shared: Arc<Shared>,
}
impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMicrophoneAuthority")
            .finish_non_exhaustive()
    }
}
impl Authority {
    /// `approved` is separate local microphone consent for this session and
    /// selected endpoint. A control grant, observation approval or playback
    /// audio permission must never be substituted for it. `now` samples the
    /// same host clock used by this original `InputSession`, once on the caller.
    pub fn from_input(
        input: &InputSession,
        channel: Channel,
        selection: &Selection,
        approved: bool,
        now: impl FnOnce() -> HostInstant,
    ) -> Result<Self, Error> {
        if !approved {
            return Err(Error::Permission);
        }
        if !fr_wire::audio_up::uplink_selected(selection) {
            return Err(Error::NotSelected);
        }
        let scope = input.ticket_credentials(InputTicketId::from_raw(0));
        if channel.parent.remote_session != scope.session
            || scope.lease.as_raw() == 0
            || channel.parent.id == 0
            || channel.parent.host_boot.as_raw() == 0
            || channel.parent.os_session.as_raw() == 0
            || channel.parent.remote_session.as_raw() == 0
            || channel.id == 0
            || channel.id == channel.parent.id
            || channel.packet_maximum <= audio::AUDIO_PACKET_OVERHEAD
            || channel.packet_maximum > audio::AUDIO_PACKET_OVERHEAD + MAX_OPUS_PAYLOAD_BYTES
        {
            return Err(Error::Binding);
        }
        let owner = Self {
            shared: Arc::new(Shared {
                channel,
                lease: scope.lease,
                monitor: input.monitor(),
                clock: Clock::bind(now)?,
                stopped: AtomicBool::new(false),
                native: Mutex::new(Native::default()),
            }),
        };
        owner.shared.check()?;
        Ok(owner)
    }
    pub fn control(&self) -> Control {
        Control(self.shared.clone())
    }
    pub fn channel(&self) -> Channel {
        self.shared.channel
    }
    pub fn lease(&self) -> InputLeaseId {
        self.shared.lease
    }
}
impl Drop for Authority {
    fn drop(&mut self) {
        self.shared.stop();
    }
}

/// One acknowledged uplink generation on a real qualified local virtual source.
/// No raw decoder/native access and no permission callback supplied by a packet.
/// A later generation needs a new explicitly approved owner after old cleanup;
/// the containing channel coordinator retains the generation floor and one-use
/// attachment. This owner neither advertises nor installs a virtual driver.
pub struct Receiver {
    authority: Authority,
    uplink: Uplink,
}
impl std::fmt::Debug for Receiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostMicrophoneReceiver")
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}
impl Receiver {
    /// Validate the exact configuration binding, native Opus profile and real
    /// datagram ceiling before opening a native endpoint or decoder child.
    pub fn new(
        authority: Authority,
        bytes: &[u8],
        server: &Path,
        decoder_image: &Path,
    ) -> Result<Self, Error> {
        authority.shared.check()?;
        let channel = authority.shared.channel;
        let offer = audio::decode_configuration(bytes, channel.id).map_err(Error::Wire)?;
        if offer.direction != AudioDirection::Uplink
            || offer.generation.as_raw() == 0
            || !matches!(offer.frame_duration_ms, 10 | 20)
            || audio::packet_record_bytes(
                usize::try_from(offer.max_packet_bytes).map_err(|_| Error::Configuration)?,
            )
            .is_none_or(|size| size > channel.packet_maximum)
        {
            return Err(Error::Configuration);
        }
        let config = AudioStreamConfig::new(
            offer.direction,
            offer.generation,
            offer.channels,
            offer.frame_duration_ms,
            offer.jitter_target_ms,
        )
        .map_err(|_| Error::Configuration)?;
        if config.expected_samples_per_frame() > offer.max_decoded_samples {
            return Err(Error::Configuration);
        }
        let control = authority.control();
        let device =
            Microphone::connect(server, config, || control.permitted()).map_err(Error::Device)?;
        authority.shared.bind_device(device.control())?;
        let uplink = Uplink::new(channel.id, offer, device, decoder_image, || {
            control.permitted()
        })
        .map_err(Error::Uplink)?;
        authority.shared.bind_decoder(uplink.decoder_retirement())?;
        Ok(Self { authority, uplink })
    }
    pub fn control(&self) -> Control {
        self.authority.control()
    }
    pub fn decoder_retirement(&self) -> Retirement {
        self.uplink.decoder_retirement()
    }
    pub fn configuration(&self) -> AudioConfiguration {
        self.uplink.configuration()
    }
    /// Physical endpoint state, updated by its native worker. A policy fence
    /// can precede graph destruction; `Control::is_stopped` reports that fence
    /// and samples original authority even while this worker is idle.
    pub fn state(&self) -> State {
        self.uplink.state()
    }
    pub fn statistics(&self) -> Statistics {
        self.uplink.statistics()
    }
    pub fn queued_packets(&self) -> usize {
        self.uplink.queued_packets()
    }
    fn check(&mut self) -> Result<(), Error> {
        match self.authority.shared.check() {
            Ok(_) => Ok(()),
            Err(error) => {
                self.stop();
                Err(error)
            }
        }
    }
    fn result<T>(&mut self, result: Result<T, uplink::Error>) -> Result<T, Error> {
        match result {
            Ok(value) => {
                self.check()?;
                Ok(value)
            }
            Err(error) => {
                self.stop();
                Err(Error::Uplink(error))
            }
        }
    }
    pub fn poll_ready(&mut self) -> Result<bool, Error> {
        self.check()?;
        let control = self.control();
        let result = self.uplink.poll_ready(|| control.permitted());
        self.result(result)
    }
    /// Exactly one positive reply, after the actual endpoint and decoder are
    /// ready. Callback success means original-route transport admission, not
    /// merely custody in a worker outbox. Unknown outcome ends this generation.
    pub fn acknowledge(&mut self, send: impl FnOnce(&[u8]) -> Result<(), ()>) -> Result<(), Error> {
        self.check()?;
        let control = self.control();
        let result = self.uplink.acknowledge(|| control.permitted(), send);
        self.result(result)
    }
    /// `arrived` is the ORIGINAL local application receipt before every inbox.
    /// The existing `Uplink` keeps its inclusive 100ms expiry through jitter,
    /// decode and actual graph submission. A later dequeue never supplies now.
    /// The transport coordinator must preserve any shorter negotiated expiry.
    pub fn receive_record_at(
        &mut self,
        bytes: &[u8],
        arrived: Instant,
    ) -> Result<ReceiveResult, Error> {
        // A bound stop fences without sampling the clock or waiting for renewed
        // authority. Permission loss must never delay this terminal operation.
        if let Ok(stop) = audio::decode_stop(bytes, self.authority.shared.channel.id)
            && stop.direction == AudioDirection::Uplink
            && stop.generation == self.uplink.configuration().generation
        {
            self.stop();
            return Ok(ReceiveResult::Stopped(stop.reason));
        }
        self.check()?;
        if bytes.len() > self.authority.shared.channel.packet_maximum {
            self.stop();
            return Err(Error::Configuration);
        }
        let control = self.control();
        let result = self
            .uplink
            .receive_record_at(bytes, arrived, || control.permitted());
        self.result(result)
    }
    pub fn service(&mut self) -> Result<RenderResult, Error> {
        self.check()?;
        let control = self.control();
        let result = self.uplink.service(|| control.permitted());
        self.result(result)
    }
    /// Fence original native output before clearing retained packets or entering
    /// foreign destruction. Input and unrelated optional lanes remain live.
    pub fn stop(&mut self) {
        self.authority.shared.stop();
        self.uplink.stop();
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop();
    }
}
