#![forbid(unsafe_code)]
//! One explicit microphone lane on the original accepted controller.
//!
//! Native capture/Opus encoding stays in its supervised process. This owner
//! admits its output only after the exact host configuration acknowledgement,
//! while separate host approval, OS permission, fresh talk consent and the
//! original controller remain live. No callback or native work runs under an
//! authority lock. One bounded packet is retained, with its original deadline.
mod capture;
#[cfg(test)]
mod tests;

use crate::{
    clipboard::{ProjectedClock, projection::Owner},
    input::{ClientInstant, InputClient},
};
pub use capture::{CaptureMonitor, CapturePermit, CaptureReceipt};
use fr_core::{
    audio::{
        AudioChannels, AudioDirection, AudioGeneration, AudioStopReason, AudioStreamConfig,
        MAX_OPUS_PAYLOAD_BYTES, MicPermission, MicTalkMode, OPUS_SAMPLE_RATE,
    },
    clipboard::authority::Monitor,
    ids::InputLeaseId,
};
use fr_media::audio::AudioAccessUnit;
use fr_wire::{
    WireError,
    audio::{self, AudioConfiguration, AudioStop},
    negotiation::{ControlBinding, Selection},
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

pub const MAX_CAPTURE_CUSTODY_US: u64 = 100_000;
pub const MAX_CAPTURE_PACKETS: u8 = 4;
pub const CONFIGURATION_TIMEOUT_US: u64 = 2_000_000;
const RENEW_MARGIN_US: u64 = 1_000_000;
const RENEW_INTERVAL_US: u64 = 250_000;
const RENEW_ADVANCE_US: u64 = 100_000;
const MAX_RECORD_BYTES: usize = audio::AUDIO_PACKET_OVERHEAD + MAX_OPUS_PAYLOAD_BYTES;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotGranted,
    NotSelected,
    Permission,
    WrongScope,
    Configuration,
    NotTalking,
    Order,
    Clock,
    Expired,
    Retired,
    Stopped,
    Backpressure,
    InvalidPacket,
    CounterExhausted,
    Transport,
    Authority(crate::clipboard::Error),
    Wire(WireError),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "microphone-uplink: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Actual negotiated wire allowance and locally selected native Opus format.
/// Device selection/qualification and host approval are separate inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub channels: AudioChannels,
    pub frame_duration_ms: u16,
    pub max_packet_bytes: u16,
    pub max_record_bytes: usize,
    pub jitter_target_ms: u16,
}
impl Profile {
    fn configuration(self, generation: AudioGeneration) -> Result<AudioConfiguration, Error> {
        if !matches!(self.frame_duration_ms, 10 | 20)
            || self.max_packet_bytes == 0
            || usize::from(self.max_packet_bytes) > MAX_OPUS_PAYLOAD_BYTES
            || self.max_record_bytes > MAX_RECORD_BYTES
            || audio::packet_record_bytes(usize::from(self.max_packet_bytes))
                .is_none_or(|bytes| bytes > self.max_record_bytes)
        {
            return Err(Error::Configuration);
        }
        let config = AudioConfiguration {
            direction: AudioDirection::Uplink,
            generation,
            channels: self.channels,
            sample_rate: OPUS_SAMPLE_RATE,
            frame_duration_ms: self.frame_duration_ms,
            max_packet_bytes: u32::from(self.max_packet_bytes),
            max_decoded_samples: OPUS_SAMPLE_RATE / 1000 * u32::from(self.frame_duration_ms),
            jitter_target_ms: self.jitter_target_ms,
        };
        config.validate().map_err(Error::Wire)?;
        Ok(config)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Muted,
    Configuring,
    AwaitingHost,
    Capturing,
    Stopped,
}
struct Epoch {
    live: AtomicBool,
    until: Mutex<ClientInstant>,
    configuration: AudioConfiguration,
}
impl Epoch {
    fn stop(&self) {
        self.live.store(false, Ordering::Release);
    }
    fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
}

/// Unique sender for one separately approved authenticated `AudioUp` attachment.
/// Drop fences every worker permit before releasing retained speech. A new
/// sender cannot be attached again to the same original input owner.
pub struct Sender {
    parent: ControlBinding,
    lease: InputLeaseId,
    channel: u32,
    profile: Profile,
    monitor: Monitor,
    clock: ProjectedClock,
    last_now: ClientInstant,
    permission: MicPermission,
    status: Status,
    generation: AudioGeneration,
    epoch: Option<Arc<Epoch>>,
    configuration_until: Option<ClientInstant>,
    refresh_after: ClientInstant,
    configuration_bytes: [u8; audio::AUDIO_CONFIGURATION_RECORD_BYTES],
    next: Option<(u64, u64)>,
    pending_len: usize,
    pending_until: Option<ClientInstant>,
    pending_bytes: [u8; MAX_RECORD_BYTES],
}
impl std::fmt::Debug for Sender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MicrophoneSender")
            .field("status", &self.status())
            .field("permission", &self.permission)
            .field("pending_bytes", &self.pending_len)
            .finish_non_exhaustive()
    }
}
impl InputClient {
    /// Join only a completed `AudioUp` route from this exact original session.
    /// `host_granted` is the independently approved microphone capability and
    /// qualified endpoint, never clipboard approval or the input grant alone.
    /// No capture starts here; the new lane remains muted with unknown OS grant.
    #[allow(clippy::too_many_arguments)]
    pub fn attach_microphone(
        &mut self,
        parent: ControlBinding,
        channel: u32,
        selection: &Selection,
        host_granted: bool,
        profile: Profile,
        now: ClientInstant,
    ) -> Result<Sender, Error> {
        if !host_granted {
            return Err(Error::Permission);
        }
        if !fr_wire::audio_up::uplink_selected(selection) {
            return Err(Error::NotSelected);
        }
        profile.configuration(AudioGeneration::from_raw(1))?;
        self.tick(now).map_err(|_| Error::Stopped)?;
        let projection: &mut Owner = self
            .microphone_projection
            .as_mut()
            .ok_or(Error::NotGranted)?;
        if parent != projection.parent() {
            return Err(Error::WrongScope);
        }
        let lease = projection.binding().lease;
        let (monitor, clock) = projection
            .attach(channel, now, false)
            .map_err(Error::Authority)?;
        Ok(Sender {
            parent,
            lease,
            channel,
            profile,
            monitor,
            clock,
            last_now: now,
            permission: MicPermission::NotRequested,
            status: Status::Muted,
            generation: AudioGeneration::INITIAL,
            epoch: None,
            configuration_until: None,
            refresh_after: now,
            configuration_bytes: [0; audio::AUDIO_CONFIGURATION_RECORD_BYTES],
            next: None,
            pending_len: 0,
            pending_until: None,
            pending_bytes: [0; MAX_RECORD_BYTES],
        })
    }
}
impl Sender {
    pub const fn parent(&self) -> ControlBinding {
        self.parent
    }
    pub const fn lease(&self) -> InputLeaseId {
        self.lease
    }
    pub const fn channel(&self) -> u32 {
        self.channel
    }
    pub const fn profile(&self) -> Profile {
        self.profile
    }
    pub const fn permission(&self) -> MicPermission {
        self.permission
    }
    pub fn status(&self) -> Status {
        if self.status != Status::Stopped
            && self.epoch.as_ref().is_some_and(|epoch| !epoch.is_live())
        {
            Status::Muted
        } else {
            self.status
        }
    }
    pub const fn has_pending_record(&self) -> bool {
        self.pending_len != 0
    }
    pub const fn configuration_deadline(&self) -> Option<ClientInstant> {
        self.configuration_until
    }

    /// Unknown permission is permission loss. A later grant cannot restore a
    /// previous talk mode, an acknowledged configuration, or a worker permit.
    pub fn set_permission(&mut self, permission: MicPermission) {
        self.permission = permission;
        if permission != MicPermission::Granted {
            self.retire_epoch();
        }
    }
    fn retire_epoch(&mut self) {
        if let Some(epoch) = self.epoch.take() {
            epoch.stop();
        }
        self.configuration_bytes.fill(0);
        self.configuration_until = None;
        self.pending_bytes.fill(0);
        self.pending_len = 0;
        self.pending_until = None;
        self.next = None;
        if self.status != Status::Stopped {
            self.status = Status::Muted;
        }
    }
    /// Local mute fences immediately; notification is optional transport work
    /// afterward. A fresh explicit talk action uses a strictly newer generation.
    pub fn mute(&mut self) -> Option<AudioStop> {
        let notification = self.epoch.as_ref().map(|epoch| AudioStop {
            direction: AudioDirection::Uplink,
            generation: epoch.configuration.generation,
            reason: AudioStopReason::UserMute,
        });
        self.retire_epoch();
        notification
    }
    pub fn device_changed(&mut self) -> Option<AudioStop> {
        let mut notification = self.mute();
        self.permission = MicPermission::NotRequested;
        if let Some(stop) = &mut notification {
            stop.reason = AudioStopReason::DeviceChanged;
        }
        notification
    }
    /// Host grant loss, disconnection and terminal transport failures end this
    /// optional lane only. They never revoke clipboard, files or input authority.
    pub fn stop(&mut self) {
        self.retire_epoch();
        self.monitor.revoke();
        self.status = Status::Stopped;
    }
    fn fail<T>(&mut self, error: Error) -> Result<T, Error> {
        self.stop();
        Err(error)
    }
    fn check(&mut self, now: ClientInstant) -> Result<(), Error> {
        if self.status == Status::Stopped {
            return Err(Error::Stopped);
        }
        if now < self.last_now {
            return self.fail(Error::Clock);
        }
        self.last_now = now;
        if let Err(error) = self.clock.sample_concurrent(now) {
            self.retire_epoch();
            return Err(Error::Authority(error));
        }
        if self.epoch.as_ref().is_some_and(|epoch| !epoch.is_live()) {
            self.retire_epoch();
            return Err(Error::Retired);
        }
        if self.configuration_until.is_some_and(|until| now >= until)
            && matches!(self.status, Status::Configuring | Status::AwaitingHost)
        {
            self.retire_epoch();
            return Err(Error::Expired);
        }
        Ok(())
    }
    /// A local user action, never a network callback. No PCM is admitted while
    /// configuration is queued/backpressured or awaiting its exact host reply.
    pub fn begin_talk(
        &mut self,
        mode: MicTalkMode,
        now: ClientInstant,
    ) -> Result<AudioConfiguration, Error> {
        self.check(now)?;
        if self.permission != MicPermission::Granted {
            return Err(Error::Permission);
        }
        if !mode.is_active() {
            return Err(Error::NotTalking);
        }
        if self.status != Status::Muted {
            return Err(Error::Order);
        }
        let Some(generation) = self.generation.next() else {
            return self.fail(Error::CounterExhausted);
        };
        // Every attempted epoch is consumed, including local setup failure.
        self.generation = generation;
        let configuration = self.profile.configuration(generation)?;
        let cap = self.clock.local_deadline(now).map_err(Error::Authority)?;
        let until = ClientInstant(
            now.0
                .checked_add(CONFIGURATION_TIMEOUT_US)
                .ok_or(Error::Clock)?
                .min(cap.0),
        );
        audio::encode_configuration(&configuration, self.channel, &mut self.configuration_bytes)
            .map_err(Error::Wire)?;
        self.epoch = Some(Arc::new(Epoch {
            live: AtomicBool::new(true),
            until: Mutex::new(until),
            configuration,
        }));
        self.configuration_until = Some(until);
        self.status = Status::Configuring;
        Ok(configuration)
    }
    pub fn pending_configuration(&mut self, now: ClientInstant) -> Result<Option<&[u8]>, Error> {
        self.check(now)?;
        Ok((self.status == Status::Configuring).then_some(self.configuration_bytes.as_slice()))
    }
    pub fn configuration_sent(&mut self, now: ClientInstant) -> Result<(), Error> {
        self.check(now)?;
        if self.status != Status::Configuring {
            return self.fail(Error::Order);
        }
        self.status = Status::AwaitingHost;
        self.configuration_bytes.fill(0);
        Ok(())
    }
    /// Only the coordinator's authenticated `AudioUp` reply route may call this.
    /// Every field must match; a rejected/other-epoch acknowledgement never starts
    /// a source and never substitutes for fresh local talk consent.
    pub fn configured(&mut self, bytes: &[u8], now: ClientInstant) -> Result<CapturePermit, Error> {
        self.check(now)?;
        if self.status != Status::AwaitingHost {
            return self.fail(Error::Order);
        }
        let ack = match audio::decode_configured(bytes, self.channel) {
            Ok(ack) => ack,
            Err(error) => return self.fail(Error::Wire(error)),
        };
        let epoch = self.epoch.as_ref().ok_or(Error::Retired)?;
        let cfg = epoch.configuration;
        if !ack.accepted
            || ack.direction != AudioDirection::Uplink
            || ack.generation != cfg.generation
            || ack.actual_channels != cfg.channels
            || ack.actual_sample_rate != cfg.sample_rate
            || ack.actual_frame_duration_ms != cfg.frame_duration_ms
        {
            return self.fail(Error::Configuration);
        }
        let until = self.clock.local_deadline(now).map_err(Error::Authority)?;
        let configuration = AudioStreamConfig::new(
            AudioDirection::Uplink,
            cfg.generation,
            cfg.channels,
            cfg.frame_duration_ms,
            cfg.jitter_target_ms,
        )
        .map_err(|_| Error::Configuration)?;
        *epoch.until.lock().map_err(|_| Error::Retired)? = until;
        let permit = CapturePermit {
            epoch: Arc::downgrade(epoch),
            clock: self.clock.clone(),
            configuration,
            until,
        };
        permit.check(now)?;
        self.status = Status::Capturing;
        self.configuration_until = None;
        Ok(permit)
    }
    /// Mint a successor only from a still-live capture proof of this same talk
    /// epoch. Only accepted original-input tickets can advance its authority.
    /// Existing permits, captures and queued records keep their original caps.
    /// Extensions are admitted at most four times per second, within the last
    /// second of the old proof, for at least another 100ms of authority; tighter
    /// authority is applied without that delay. Clock rounding is not a renewal.
    /// Native owners must install and acknowledge the successor before the OLD
    /// cap expires. A failed/unknown native renewal must stop this sender.
    pub fn refresh_capture(
        &mut self,
        previous: &CapturePermit,
        now: ClientInstant,
    ) -> Result<Option<CapturePermit>, Error> {
        self.check(now)?;
        if self.status != Status::Capturing || self.permission != MicPermission::Granted {
            return Err(Error::NotTalking);
        }
        let epoch = self.epoch.as_ref().ok_or(Error::Retired)?;
        if !previous.belongs_to(epoch) {
            return Err(Error::WrongScope);
        }
        previous.check(now)?;
        let until = self.clock.local_deadline(now).map_err(Error::Authority)?;
        // A newer clock/proof cannot rescue a predecessor which expired while
        // that proof was being sampled. Check its effective ordered time again.
        previous.check(now)?;
        let mut current = epoch.until.lock().map_err(|_| Error::Retired)?;
        if !epoch.is_live() {
            return Err(Error::Retired);
        }
        if *current != previous.until {
            return Err(Error::Order);
        }
        if until == previous.until
            || (until > previous.until
                && (now < self.refresh_after
                    || until.0 - previous.until.0 < RENEW_ADVANCE_US
                    || previous.until.0.saturating_sub(now.0) > RENEW_MARGIN_US))
        {
            return Ok(None);
        }
        if now >= previous.until || now >= until {
            epoch.stop();
            return Err(Error::Expired);
        }
        let refresh_after =
            ClientInstant(now.0.checked_add(RENEW_INTERVAL_US).ok_or(Error::Clock)?);
        *current = until;
        drop(current);
        self.refresh_after = refresh_after;
        Ok(Some(CapturePermit {
            epoch: Arc::downgrade(epoch),
            clock: self.clock.clone(),
            configuration: previous.configuration,
            until,
        }))
    }
    pub fn receive_stop(&mut self, bytes: &[u8], now: ClientInstant) -> Result<(), Error> {
        self.check(now)?;
        let stop = match audio::decode_stop(bytes, self.channel) {
            Ok(stop) => stop,
            Err(error) => return self.fail(Error::Wire(error)),
        };
        if stop.direction != AudioDirection::Uplink
            || self
                .epoch
                .as_ref()
                .is_none_or(|epoch| stop.generation != epoch.configuration.generation)
        {
            return self.fail(Error::WrongScope);
        }
        self.stop();
        Ok(())
    }
    /// Reserve original custody BEFORE requesting a native batch. The native
    /// reply may shorten it for older partial PCM; it must never restart it.
    pub fn begin_capture(
        &mut self,
        permit: &CapturePermit,
        now: ClientInstant,
        expires: ClientInstant,
    ) -> Result<CaptureReceipt, Error> {
        self.check(now)?;
        permit.check(now)?;
        if self.status != Status::Capturing {
            return Err(Error::NotTalking);
        }
        if self.has_pending_record() {
            return Err(Error::Backpressure);
        }
        let epoch = self.epoch.as_ref().ok_or(Error::Retired)?;
        if !permit.belongs_to(epoch) {
            return self.fail(Error::WrongScope);
        }
        if expires <= now || expires > permit.until || expires.0 - now.0 > MAX_CAPTURE_CUSTODY_US {
            return Err(Error::Expired);
        }
        Ok(CaptureReceipt {
            epoch: Arc::downgrade(epoch),
            acquired: now,
            expires,
            packets: 0,
        })
    }
    /// Admit already encoded native output. Metadata must continue this exact
    /// source timeline; no lossy repair, retiming, encoding or deferred work here.
    pub fn submit(
        &mut self,
        receipt: &mut CaptureReceipt,
        packet: &AudioAccessUnit,
        now: ClientInstant,
    ) -> Result<(), Error> {
        self.check(now)?;
        if self.status != Status::Capturing {
            return Err(Error::NotTalking);
        }
        if self.has_pending_record() {
            return Err(Error::Backpressure);
        }
        let epoch = self.epoch.as_ref().ok_or(Error::Retired)?;
        if !receipt.belongs_to(epoch) {
            return self.fail(Error::WrongScope);
        }
        if now < receipt.acquired || now >= receipt.expires {
            self.retire_epoch();
            return Err(Error::Expired);
        }
        if receipt.packets >= MAX_CAPTURE_PACKETS {
            return self.fail(Error::InvalidPacket);
        }
        let cfg = epoch.configuration;
        if packet.direction() != AudioDirection::Uplink
            || packet.generation() != cfg.generation
            || u32::from(packet.duration_samples()) != cfg.max_decoded_samples
            || packet.payload().len() > usize::from(self.profile.max_packet_bytes)
            || self.next.map_or(packet.sequence() != 0, |next| {
                next != (packet.sequence(), packet.timestamp_samples())
            })
        {
            return self.fail(Error::InvalidPacket);
        }
        let Some(sequence) = packet.sequence().checked_add(1) else {
            return self.fail(Error::CounterExhausted);
        };
        let Some(timestamp) = packet
            .timestamp_samples()
            .checked_add(u64::from(packet.duration_samples()))
        else {
            return self.fail(Error::CounterExhausted);
        };
        let record = audio::AudioPacket {
            direction: AudioDirection::Uplink,
            generation: cfg.generation,
            sequence: packet.sequence(),
            timestamp_samples: packet.timestamp_samples(),
            duration_samples: packet.duration_samples(),
            payload: packet.payload(),
        };
        self.pending_len = match audio::encode_packet(
            &record,
            self.channel,
            &mut self.pending_bytes[..self.profile.max_record_bytes],
        ) {
            Ok(length) => length,
            Err(error) => return self.fail(Error::Wire(error)),
        };
        self.next = Some((sequence, timestamp));
        receipt.packets += 1;
        self.pending_until = Some(receipt.expires);
        Ok(())
    }
    /// Recheck immediately before synchronous transport admission. `false`
    /// retains identical bytes and deadline. Accepted bytes remain subject to
    /// the transport's original permit check until actual send; the callback
    /// must carry the same worker/authority fence across any deferred handoff.
    pub fn try_send(
        &mut self,
        now: ClientInstant,
        send: impl FnOnce(&[u8], ClientInstant) -> Result<bool, ()>,
    ) -> Result<bool, Error> {
        self.check(now)?;
        let Some(until) = self.pending_until else {
            return Ok(false);
        };
        if now >= until {
            self.retire_epoch();
            return Err(Error::Expired);
        }
        // A caught callback panic still fences the source and retained packet.
        let mut call = SendCall {
            sender: self,
            complete: false,
        };
        let accepted = send(&call.sender.pending_bytes[..call.sender.pending_len], until)
            .map_err(|()| Error::Transport)?;
        if accepted {
            call.sender.pending_bytes.fill(0);
            call.sender.pending_len = 0;
            call.sender.pending_until = None;
        }
        call.sender.check(now)?;
        call.complete = true;
        Ok(accepted)
    }
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.stop();
    }
}
struct SendCall<'a> {
    sender: &'a mut Sender,
    complete: bool,
}
impl Drop for SendCall<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.sender.stop();
        }
    }
}
