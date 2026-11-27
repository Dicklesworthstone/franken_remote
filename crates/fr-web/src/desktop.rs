//! Authenticated WSS view-only desktop composition. This owns the original
//! receive reservations and callback identities; BrowserSessionCore remains the
//! sole session, HEVC and cumulative-credit owner. JavaScript performs I/O only.
use crate::{
    BrowserSessionCore,
    adapter::BrowserAdapterError as Error,
    hevc::{BrowserDecoderStage, DecoderInstance},
    media_adapter::{
        MediaSnapshot, OutputReceipt, PreparedChunk, PreparedConfiguration, ReleaseReceipt,
    },
};
use fr_core::limits::ProtocolLimits;
use fr_media::delivery::{
    DecodedFrame, DecoderBinding, MediaBindings, MediaBudget, MediaEpoch, ReceiveConfig,
    ReceivePipeline, ReceivePolicy, ReceivedPicture,
};
use fr_media::freshness::ViewTracker;
use fr_wire::{
    Channel, FrameDescriptor, Kind, MediaLimits, Record, authority, decoder,
    input::{
        InputDelivery::Reliable,
        InputDirection::{HostToViewer, ViewerToHost},
    },
    negotiation::{
        ControlBinding, Message, Offer, Role, Selection,
        browser::{self, attachment},
    },
};
use serde::Serialize;
use std::collections::VecDeque;

const MAX_CONTROL_QUEUE: usize = 8;
const MAX_VIDEO_FEEDBACK: usize = 2;
const MAX_CALLBACK: u64 = 9_007_199_254_740_991;
const CLOCK_INTERVAL_US: u64 = 1_000_000;

impl From<fr_wire::WireError> for Error {
    fn from(_: fr_wire::WireError) -> Self {
        Self::InvalidMedia
    }
}
impl From<fr_wire::negotiation::Error> for Error {
    fn from(_: fr_wire::negotiation::Error) -> Self {
        Self::InvalidControl
    }
}
impl From<fr_media::delivery::DeliveryError> for Error {
    fn from(_: fr_media::delivery::DeliveryError) -> Self {
        Self::InvalidMedia
    }
}
impl From<crate::hevc::BrowserHevcError> for Error {
    fn from(_: crate::hevc::BrowserHevcError) -> Self {
        Self::InvalidMedia
    }
}
impl From<crate::media_adapter::MediaAdapterError> for Error {
    fn from(_: crate::media_adapter::MediaAdapterError) -> Self {
        Self::InvalidMedia
    }
}

struct QueuedControl {
    bytes: Vec<u8>,
    clock: bool,
    pressure: bool,
}
struct Reservation {
    descriptor: FrameDescriptor,
    charge: u64,
    submitted: bool,
}
struct NativeFrame {
    callback: u64,
    decoder: DecoderInstance,
    picture: ReceivedPicture,
    escaped: bool,
    decoded: Option<DecodedFrame>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationParameters<'a> {
    generation: String,
    coded_width: u32,
    coded_height: u32,
    crop_width: u32,
    crop_height: u32,
    fps: u16,
    primaries: u8,
    transfer: u8,
    matrix: u8,
    full_range: bool,
    decoded_pictures: u8,
    codec: &'a str,
    hvcc: &'a [u8],
}

pub(crate) struct DesktopSession {
    offer: Offer,
    host: Option<Offer>,
    selected: Option<Selection>,
    binding: Option<ControlBinding>,
    descriptor: Option<attachment::Descriptor>,
    grant: Option<attachment::Grant>,
    attached: bool,
    attach_taken: bool,
    configuration: Option<Vec<u8>>,
    decoder: Option<DecoderInstance>,
    receiver: Option<ReceivePipeline>,
    receiver_decoder: Option<DecoderBinding>,
    view: Option<ViewTracker>,
    reservations: Vec<Reservation>,
    frames: Vec<NativeFrame>,
    last_announced: Option<FrameDescriptor>,
    next_callback: u64,
    control: VecDeque<QueuedControl>,
    control_taken: Option<bool>,
    video_feedback: VecDeque<Vec<u8>>,
    next_clock_us: u64,
    first_decoded: bool,
    closed: bool,
    failed: bool,
    quarantined: bool,
}

impl DesktopSession {
    pub fn new(offer: Offer) -> Self {
        Self {
            offer,
            host: None,
            selected: None,
            binding: None,
            descriptor: None,
            grant: None,
            attached: false,
            attach_taken: false,
            configuration: None,
            decoder: None,
            receiver: None,
            receiver_decoder: None,
            view: None,
            reservations: Vec::with_capacity(4),
            frames: Vec::with_capacity(4),
            last_announced: None,
            next_callback: 0,
            control: VecDeque::with_capacity(MAX_CONTROL_QUEUE),
            control_taken: None,
            video_feedback: VecDeque::with_capacity(MAX_VIDEO_FEEDBACK),
            next_clock_us: 0,
            first_decoded: false,
            closed: false,
            failed: false,
            quarantined: false,
        }
    }
    pub fn control_binding(&self) -> u32 {
        self.binding.map_or(0, |b| b.id)
    }
    pub fn ready(&self) -> bool {
        self.first_decoded && !self.closed
    }
    pub fn opened(&self) -> bool {
        self.binding.is_some() && !self.closed
    }
    fn live(&self) -> Result<(), Error> {
        if self.quarantined {
            Err(Error::MediaCleanup)
        } else if self.closed || self.failed {
            Err(Error::InvalidMedia)
        } else {
            Ok(())
        }
    }
    fn limits(&self) -> ProtocolLimits {
        self.selected
            .as_ref()
            .map_or(self.offer.limits, |s| s.limits)
    }
    fn parent(&self) -> Result<ControlBinding, Error> {
        self.binding.ok_or(Error::InvalidState)
    }
    fn media_binding(&self) -> Result<decoder::Binding, Error> {
        self.descriptor
            .map(|d| d.binding)
            .ok_or(Error::InvalidState)
    }
    fn media_limits(&self) -> Result<MediaLimits, Error> {
        Ok(MediaLimits::new(
            self.limits(),
            browser::MAX_VIDEO_RECORD_BYTES.min(self.limits().max_control_message_bytes() as usize),
            fr_wire::MAX_FRAGMENTS,
            fr_wire::MAX_REPAIR_RANGES,
        )?)
    }
    fn queue_control(&mut self, bytes: Vec<u8>, clock: bool, pressure: bool) -> Result<(), Error> {
        if bytes.len() > crate::adapter::MAX_CONTROL_RECORD_BYTES {
            return Err(Error::MessageTooLarge);
        }
        if pressure {
            if let Some(queued) = self.control.iter_mut().find(|queued| queued.pressure) {
                queued.bytes = bytes;
                return Ok(());
            }
        }
        if self.control.len() >= MAX_CONTROL_QUEUE {
            return Err(Error::QueueFull);
        }
        self.control.push_back(QueuedControl {
            bytes,
            clock,
            pressure,
        });
        Ok(())
    }
    fn queue_negotiation(&mut self, message: &Message) -> Result<(), Error> {
        let mut bytes = [0; browser::MAX_RECORD];
        let n = browser::encode(message, bytes.len(), &mut bytes)?;
        self.queue_control(bytes[..n].to_vec(), false, false)
    }
    fn queue_attachment(&mut self, message: attachment::Message) -> Result<(), Error> {
        let mut bytes = [0; attachment::GRANT_RECORD_BYTES];
        let n = attachment::encode(
            message,
            self.parent()?,
            &self.limits(),
            &mut bytes,
            ViewerToHost,
            Reliable,
        )?;
        self.queue_control(bytes[..n].to_vec(), false, false)
    }
    fn queue_video(&mut self, message: decoder::Message<'_>) -> Result<(), Error> {
        if self.video_feedback.len() >= MAX_VIDEO_FEEDBACK {
            return Err(Error::QueueFull);
        }
        let mut bytes = [0; decoder::FIRST_DECODED_BYTES];
        let n = decoder::encode(
            message,
            self.media_binding()?,
            &self.limits(),
            &mut bytes,
            ViewerToHost,
            Reliable,
        )?;
        self.video_feedback.push_back(bytes[..n].to_vec());
        Ok(())
    }

    pub fn receive_control(
        &mut self,
        core: &mut BrowserSessionCore,
        bytes: &[u8],
        now: u64,
    ) -> Result<(), Error> {
        self.live()?;
        let kind = browser::record_kind(
            bytes,
            crate::adapter::MAX_CONTROL_RECORD_BYTES,
            self.control_binding(),
        )?;
        if self.binding.is_none() {
            if !matches!(
                kind,
                Kind::HostCapabilities | Kind::ApprovalRequired | Kind::SessionOpened
            ) {
                return Err(Error::UnsupportedControl);
            }
            match browser::decode(bytes, browser::MAX_RECORD, 0)? {
                Message::HostCapabilities(host) if self.host.is_none() => {
                    if self.offer.role != Role::Observe || host.role != Role::Observe {
                        return Err(Error::InvalidRole);
                    }
                    browser::check_host(&self.offer, &host)?;
                    let selected = browser::select(&host)?;
                    self.queue_negotiation(&Message::SelectedConfiguration(selected.clone()))?;
                    self.host = Some(host);
                    self.selected = Some(selected);
                }
                Message::ApprovalRequired {
                    request,
                    role: Role::Observe,
                    ..
                } if self.selected.is_some() && request.as_raw() != 0 => {}
                Message::SessionOpened {
                    binding, selection, ..
                } if self.selected.as_ref() == Some(&selection) => {
                    core.on_admitted_session(binding, selection.limits, now)
                        .map_err(|_| Error::InvalidControl)?;
                    self.binding = Some(binding);
                    self.next_clock_us = now;
                    self.queue_negotiation(&Message::BindingAccepted {
                        binding: binding.id,
                    })?;
                }
                _ => return Err(Error::UnsupportedControl),
            }
            return Ok(());
        }
        let parent = self.parent()?;
        match kind {
            Kind::Challenge => {
                let binding = authority::Binding {
                    channel: parent.id,
                    session: parent.remote_session,
                };
                let authority::Message::Challenge {
                    scope: authority::Scope::Observation,
                    nonce,
                    ..
                } = authority::decode(bytes, binding, &self.limits(), HostToViewer, Reliable)?
                else {
                    return Err(Error::UnsupportedControl);
                };
                let mut out = [0; authority::OBSERVATION_RESPONSE_BYTES];
                let n = authority::encode(
                    authority::Message::Response {
                        scope: authority::Scope::Observation,
                        nonce,
                    },
                    binding,
                    &self.limits(),
                    &mut out,
                    ViewerToHost,
                    Reliable,
                )?;
                self.queue_control(out[..n].to_vec(), false, false)?;
            }
            Kind::ClockReply => match core.on_clock_reply(bytes, now) {
                Ok(()) | Err(fr_client::clock::Error::LateReply) => {
                    self.sync_view(core, now)?;
                }
                Err(_) => return Err(Error::InvalidClock),
            },
            Kind::StreamBinding | Kind::ChannelTicket => {
                match attachment::decode(
                    bytes,
                    parent,
                    parent.id,
                    &self.limits(),
                    HostToViewer,
                    Reliable,
                )? {
                    attachment::Message::Binding(descriptor) if self.descriptor.is_none() => {
                        self.descriptor = Some(descriptor);
                        self.queue_attachment(attachment::Message::Accepted(
                            descriptor.binding.parent.id,
                        ))?;
                    }
                    attachment::Message::Ticket(grant)
                        if self.grant.is_none() && self.descriptor == Some(grant.descriptor) =>
                    {
                        self.grant = Some(grant);
                    }
                    _ => return Err(Error::InvalidControl),
                }
            }
            _ => return Err(Error::UnsupportedControl),
        }
        Ok(())
    }

    pub fn tick(&mut self, core: &mut BrowserSessionCore, now: u64) -> Result<(), Error> {
        self.live()?;
        if let Some(receiver) = self.receiver.as_mut() {
            // Reference expiry fences the stream; v1 has no implicit decoder
            // reset or reconnect that could conceal a lost dependency chain.
            if self.receiver_decoder.is_some() {
                receiver.tick(now)?;
            }
        }
        self.discard_queued(core)?;
        self.sync_view(core, now)?;
        if self.binding.is_some()
            && now >= self.next_clock_us
            && self.control_taken != Some(true)
            && !self.control.iter().any(|queued| queued.clock)
        {
            match core.begin_clock_probe(now) {
                Ok(bytes) => {
                    self.queue_control(bytes, true, false)?;
                    self.next_clock_us = now
                        .checked_add(CLOCK_INTERVAL_US)
                        .ok_or(Error::InvalidClock)?;
                }
                Err(fr_client::clock::Error::Busy) => {}
                Err(_) => return Err(Error::InvalidClock),
            }
        }
        Ok(())
    }
    pub fn take_control(&mut self) -> Result<Option<Vec<u8>>, Error> {
        self.live()?;
        if self.control_taken.is_some() {
            return Err(Error::InvalidState);
        }
        Ok(self.control.pop_front().map(|queued| {
            self.control_taken = Some(queued.clock);
            queued.bytes
        }))
    }
    pub fn control_queued(&mut self, core: &mut BrowserSessionCore, now: u64) -> Result<(), Error> {
        self.live()?;
        let clock = self.control_taken.take().ok_or(Error::InvalidState)?;
        if clock {
            core.clock_probe_queued(now)
                .map_err(|_| Error::InvalidClock)?;
        }
        Ok(())
    }
    pub fn needs_video(&self) -> bool {
        self.grant.is_some() && !self.attach_taken && !self.closed
    }
    pub fn video_attach(&mut self) -> Result<Vec<u8>, Error> {
        self.live()?;
        if self.attach_taken {
            return Err(Error::InvalidState);
        }
        let grant = self.grant.ok_or(Error::InvalidState)?;
        let mut out = [0; attachment::GRANT_RECORD_BYTES];
        let n = attachment::encode(
            attachment::Message::Attach(grant),
            self.parent()?,
            &self.limits(),
            &mut out,
            ViewerToHost,
            Reliable,
        )?;
        self.attach_taken = true;
        Ok(out[..n].to_vec())
    }
    pub fn take_video_feedback(&mut self) -> Result<Option<Vec<u8>>, Error> {
        self.live()?;
        Ok(self.video_feedback.pop_front())
    }

    pub fn receive_video(
        &mut self,
        core: &mut BrowserSessionCore,
        bytes: &[u8],
        now: u64,
    ) -> Result<&'static str, Error> {
        self.live()?;
        let binding = self.media_binding()?;
        let kind = browser::record_kind(bytes, browser::MAX_VIDEO_RECORD_BYTES, binding.parent.id)?;
        if !self.attached {
            let expected = self.grant.ok_or(Error::InvalidState)?;
            if !self.attach_taken
                || attachment::decode(
                    bytes,
                    self.parent()?,
                    binding.parent.id,
                    &self.limits(),
                    HostToViewer,
                    Reliable,
                )? != attachment::Message::Attached(expected)
            {
                return Err(Error::InvalidMedia);
            }
            core.attach_video_channel(self.parent()?, binding.parent.id, expected.credit_epoch)
                .map_err(|_| Error::InvalidMedia)?;
            core.update_mapping(binding.geometry, binding.viewport);
            self.attached = true;
            return Ok("");
        }
        if kind == Kind::DecoderConfiguration {
            if self.configuration.is_some() {
                return Err(Error::InvalidMedia);
            }
            let decoder::Message::Configuration(config) =
                decoder::decode(bytes, binding, &self.limits(), HostToViewer, Reliable)?
            else {
                return Err(Error::InvalidMedia);
            };
            crate::media_adapter::checked_configuration(
                config,
                binding.configuration.as_raw(),
                self.limits(),
            )?;
            let media_limits = self.media_limits()?;
            let mut receiver = ReceivePipeline::new(
                ReceiveConfig {
                    limits: media_limits,
                    bindings: MediaBindings::browser_wss(binding.parent.id)?,
                    epoch: MediaEpoch {
                        configuration: binding.configuration,
                        recovery: binding.recovery,
                    },
                    policy: ReceivePolicy::default(),
                },
                MediaBudget::new(&self.limits())?,
            )?;
            receiver.configure_frame_rate(config.fps)?;
            self.receiver = Some(receiver);
            self.configuration = Some(bytes.to_vec());
            self.sync_view(core, now)?;
            return Ok(if self.configuration_ready() {
                "configuration"
            } else {
                ""
            });
        }
        if self.receiver_decoder.is_none() {
            return Err(Error::InvalidMedia);
        }
        let limits = self.media_limits()?;
        let (channel, descriptor) = match kind {
            Kind::Progress => {
                let record =
                    Record::decode(bytes, &limits, binding.parent.id, Channel::MediaConfig)?;
                let progress = fr_wire::decode_progress(record, &limits)?;
                if self
                    .last_announced
                    .is_none_or(|last| progress.descriptor.frame > last.frame)
                {
                    self.reserve(core, progress.descriptor)?;
                    self.last_announced = Some(progress.descriptor);
                } else if self.last_announced != Some(progress.descriptor) {
                    return Err(Error::InvalidMedia);
                }
                (Channel::MediaConfig, progress.descriptor)
            }
            Kind::Recovery => {
                let record = Record::decode(bytes, &limits, binding.parent.id, Channel::Recovery)?;
                let chunk = fr_wire::decode_recovery(record, &limits)?;
                let declared = self
                    .reservations
                    .iter()
                    .find(|r| r.descriptor.frame == chunk.frame)
                    .ok_or(Error::InvalidMedia)?
                    .descriptor;
                if declared.total_bytes != chunk.total_bytes
                    || declared.capture_micros != chunk.capture_micros
                    || declared.reference.is_some()
                {
                    return Err(Error::InvalidMedia);
                }
                (Channel::Recovery, declared)
            }
            Kind::Fragment => {
                let record = Record::decode(bytes, &limits, binding.parent.id, Channel::Video)?;
                (
                    Channel::Video,
                    fr_wire::decode_fragment(record, &limits)?.descriptor,
                )
            }
            _ => return Err(Error::InvalidMedia),
        };
        if channel != Channel::MediaConfig
            && !self.reservations.iter().any(|r| r.descriptor == descriptor)
        {
            return Err(Error::InvalidMedia);
        }
        let receiver = self.receiver.as_mut().ok_or(Error::InvalidState)?;
        receiver.receive(channel, bytes, now)?;
        self.discard_queued(core)?;
        self.sync_view(core, now)?;
        Ok("media")
    }

    fn reserve(
        &mut self,
        core: &mut BrowserSessionCore,
        descriptor: FrameDescriptor,
    ) -> Result<(), Error> {
        let grant = self.grant.ok_or(Error::InvalidState)?;
        let charge = browser::frame_charge(descriptor.total_bytes)?;
        let occupied: u64 = self.reservations.iter().map(|r| r.charge).sum();
        if self.reservations.len() >= grant.picture_allowance as usize
            || occupied
                .checked_add(charge)
                .is_none_or(|total| total > grant.byte_allowance)
        {
            return Err(Error::QueueFull);
        }
        core.reserve_video_frame(core.active_video_generation(), descriptor.frame, charge)?;
        self.reservations.push(Reservation {
            descriptor,
            charge,
            submitted: false,
        });
        Ok(())
    }
    fn pressure(
        &mut self,
        record: Option<[u8; fr_wire::receiver_pressure::RECORD_BYTES]>,
    ) -> Result<(), Error> {
        if !self.closed {
            if let Some(bytes) = record {
                self.queue_control(bytes.to_vec(), false, true)?;
            }
        }
        Ok(())
    }
    fn discard_queued(&mut self, core: &mut BrowserSessionCore) -> Result<(), Error> {
        let generation = core.active_video_generation();
        let mut i = 0;
        while i < self.reservations.len() {
            let r = &self.reservations[i];
            let retained = self
                .receiver
                .as_ref()
                .is_some_and(|receiver| receiver.retains_frame(r.descriptor.frame));
            if !r.submitted && !retained {
                let frame = self.reservations.remove(i).descriptor.frame;
                let receipt = core.discard_received_frame(generation, frame)?;
                self.pressure(receipt)?;
            } else {
                i += 1;
            }
        }
        Ok(())
    }

    fn sync_view(&mut self, core: &mut BrowserSessionCore, now: u64) -> Result<(), Error> {
        let Some(receiver) = self.receiver.as_ref() else {
            return Ok(());
        };
        if let Some(clock) = core.correlation(now) {
            if let Some(view) = self.view.as_mut() {
                if clock.received_at_us() > view.clock_correlation().received_at_us() {
                    view.synchronize(clock, now)
                        .map_err(|_| Error::InvalidMedia)?;
                }
            } else {
                self.view = Some(
                    ViewTracker::new(
                        receiver,
                        clock,
                        fr_client::input::Policy::default().view_age_us,
                        now,
                    )
                    .map_err(|_| Error::InvalidMedia)?,
                );
            }
        }
        let sample = if let Some(view) = self.view.as_mut() {
            view.observe_receiver(receiver, now)
                .map_err(|_| Error::InvalidMedia)?;
            view.presented_sample(now).ok()
        } else {
            None
        };
        core.apply_view_sample(sample, now)
            .map_err(|_| Error::InvalidClock)
    }
    pub fn configuration_ready(&self) -> bool {
        self.configuration.is_some()
            && self.view.is_some()
            && self.decoder.is_none()
            && !self.closed
    }
    pub fn hide(&mut self) {
        if let Some(view) = self.view.as_mut() {
            view.hide();
        }
    }

    pub fn configuration_parameters(&self) -> Result<ConfigurationParameters<'_>, Error> {
        let bytes = self.configuration.as_ref().ok_or(Error::InvalidState)?;
        let binding = self.media_binding()?;
        let decoder::Message::Configuration(c) =
            decoder::decode(bytes, binding, &self.limits(), HostToViewer, Reliable)?
        else {
            return Err(Error::InvalidMedia);
        };
        Ok(ConfigurationParameters {
            generation: binding.configuration.as_raw().to_string(),
            coded_width: c.coded_width,
            coded_height: c.coded_height,
            crop_width: c.crop_width,
            crop_height: c.crop_height,
            fps: c.fps,
            primaries: c.primaries,
            transfer: c.transfer,
            matrix: c.matrix,
            full_range: c.full_range,
            decoded_pictures: c.decoded_pictures,
            codec: c.codec,
            hvcc: c.hvcc,
        })
    }
    pub fn prepare_configuration(
        &mut self,
        core: &mut BrowserSessionCore,
    ) -> Result<PreparedConfiguration, Error> {
        self.live()?;
        if !self.configuration_ready() {
            return Err(Error::InvalidState);
        }
        let binding = self.media_binding()?;
        let bytes = self.configuration.as_ref().ok_or(Error::InvalidState)?;
        let decoder::Message::Configuration(c) =
            decoder::decode(bytes, binding, &self.limits(), HostToViewer, Reliable)?
        else {
            return Err(Error::InvalidMedia);
        };
        let config = crate::media_adapter::checked_configuration(
            c,
            binding.configuration.as_raw(),
            self.limits(),
        )?;
        let p = core.admit_decoder_configuration(config, c.decoded_pictures, c.hvcc)?;
        self.decoder = Some(p.decoder);
        Ok(PreparedConfiguration {
            decoder: p.decoder.serial.to_string(),
            generation: p.generation.as_raw().to_string(),
            codec: p.codec_string,
            hvcc: p.hvcc,
            coded_width: p.coded_width,
            coded_height: p.coded_height,
            crop_width: p.crop_width,
            crop_height: p.crop_height,
        })
    }
    fn decoder(&self, serial: u64) -> Result<DecoderInstance, Error> {
        self.decoder
            .filter(|d| d.serial == serial)
            .ok_or(Error::InvalidMedia)
    }
    pub fn configured(
        &mut self,
        core: &mut BrowserSessionCore,
        serial: u64,
        now: u64,
    ) -> Result<(), Error> {
        self.live()?;
        let decoder = self.decoder(serial)?;
        if self.receiver_decoder.is_some() {
            return Err(Error::InvalidState);
        }
        core.on_decoder_configured(decoder)?;
        let limits = self.limits();
        self.receiver_decoder = Some(
            self.receiver
                .as_mut()
                .ok_or(Error::InvalidState)?
                .bind_decoder(decoder.generation, &limits, now)?,
        );
        self.queue_video(decoder::Message::Configured)
    }
    pub fn take_chunk(
        &mut self,
        core: &mut BrowserSessionCore,
        now: u64,
    ) -> Result<Option<PreparedChunk>, Error> {
        self.live()?;
        let Some(decoder) = self.decoder else {
            return Ok(None);
        };
        let Some(receiver_decoder) = self.receiver_decoder.as_ref() else {
            return Ok(None);
        };
        let receiver = self.receiver.as_mut().ok_or(Error::InvalidState)?;
        receiver_decoder.check(receiver)?;
        let picture = receiver.take_decodable(now)?;
        self.discard_queued(core)?;
        let Some(picture) = picture else {
            return Ok(None);
        };
        if self.next_callback >= MAX_CALLBACK {
            return Err(Error::InvalidMedia);
        }
        self.receiver_decoder
            .as_ref()
            .ok_or(Error::InvalidState)?
            .check_picture(&picture)?;
        let d = picture.descriptor();
        let inner = core.admit_reserved_video_chunk(
            core.active_video_generation(),
            decoder,
            d.frame,
            d.capture_micros,
            d.reference.is_none(),
            picture.bytes(),
        )?;
        self.reservations
            .iter_mut()
            .find(|r| r.descriptor == d)
            .ok_or(Error::InvalidMedia)?
            .submitted = true;
        self.next_callback += 1;
        let callback = self.next_callback;
        self.frames.push(NativeFrame {
            callback,
            decoder,
            picture,
            escaped: false,
            decoded: None,
        });
        Ok(Some(PreparedChunk { callback, inner }))
    }

    pub fn output(
        &mut self,
        core: &mut BrowserSessionCore,
        serial: u64,
        callback: u64,
        now: u64,
    ) -> Result<OutputReceipt, Error> {
        if self.quarantined {
            return Err(Error::MediaCleanup);
        }
        let decoder = self.decoder(serial)?;
        let index = self
            .frames
            .iter()
            .position(|f| f.decoder == decoder && f.callback == callback)
            .ok_or(Error::InvalidMedia)?;
        let descriptor = self.frames[index].picture.descriptor();
        core.retain_video_output(decoder, descriptor.frame)?;
        self.frames[index].escaped = true;
        let mut can_present = false;
        let mut first_decoded = false;
        if !self.closed {
            let decoded = core.on_frame_decoded(decoder, descriptor.frame);
            let receive = if decoded.is_ok() {
                self.receiver
                    .as_mut()
                    .ok_or(Error::InvalidState)?
                    .complete_decode(&self.frames[index].picture, now)
            } else {
                Err(fr_media::delivery::DeliveryError::DecodeFailed)
            };
            if let Ok(receipt) = receive {
                let frame = &mut self.frames[index];
                can_present =
                    now < frame.picture.display_deadline_us() || frame.picture.is_newest();
                frame.decoded = Some(receipt);
                first_decoded = !self.first_decoded;
                if first_decoded {
                    self.first_decoded = true;
                    self.queue_video(decoder::Message::FirstDecoded {
                        frame: descriptor.frame,
                        decoder_micros: now,
                    })?;
                }
            } else {
                self.failed = true;
            }
            if self.discard_queued(core).is_err() {
                self.failed = true;
                can_present = false;
            }
        }
        Ok(OutputReceipt {
            decoder: serial.to_string(),
            generation: decoder.generation.as_raw().to_string(),
            callback,
            frame_id: descriptor.frame.to_string(),
            source_timestamp_us: descriptor.capture_micros.to_string(),
            charged_bytes: browser::frame_charge(descriptor.total_bytes)?,
            can_present,
            first_decoded,
        })
    }
    pub fn presented(
        &mut self,
        core: &mut BrowserSessionCore,
        serial: u64,
        callback: u64,
        now: u64,
        visible: bool,
    ) -> Result<(), Error> {
        self.live()?;
        if !visible {
            return Err(Error::InvalidMedia);
        }
        let decoder = self.decoder(serial)?;
        let frame = self
            .frames
            .iter_mut()
            .find(|f| f.decoder == decoder && f.callback == callback && f.escaped)
            .ok_or(Error::InvalidMedia)?;
        if !frame.picture.is_live() {
            return Err(Error::InvalidMedia);
        }
        let number = frame.picture.descriptor().frame;
        let receipt = frame.decoded.take().ok_or(Error::InvalidMedia)?;
        let view = self.view.as_mut().ok_or(Error::InvalidState)?;
        view.decoded(receipt, true, now)
            .map_err(|_| Error::InvalidMedia)?;
        // A late newest frame may be visible, but the canonical tracker keeps
        // it unqualified until genuinely newer evidence of that exact source.
        let _ = view.visible(number, now);
        core.on_frame_presented(decoder, number, now)?;
        self.sync_view(core, now)
    }
    fn release_receipt(
        &self,
        core: &BrowserSessionCore,
        decoder: DecoderInstance,
        callback: u64,
        frame: u64,
        charge: u64,
    ) -> ReleaseReceipt {
        let (_, remaining_frames, remaining_bytes) = core.media_owner_counts();
        ReleaseReceipt {
            decoder: decoder.serial.to_string(),
            generation: decoder.generation.as_raw().to_string(),
            callback,
            frame_id: frame.to_string(),
            released_bytes: charge,
            remaining_frames,
            remaining_bytes,
        }
    }
    pub fn frame_closed(
        &mut self,
        core: &mut BrowserSessionCore,
        serial: u64,
        callback: u64,
        now: u64,
    ) -> Result<ReleaseReceipt, Error> {
        if self.quarantined {
            return Err(Error::MediaCleanup);
        }
        let decoder = self.decoder(serial)?;
        let position = self
            .frames
            .iter()
            .position(|f| f.decoder == decoder && f.callback == callback && f.escaped)
            .ok_or(Error::InvalidMedia)?;
        let mut frame = self.frames.remove(position);
        if !self.closed {
            if let (Some(view), Some(receipt)) = (self.view.as_mut(), frame.decoded.take()) {
                let _ = view.decoded(receipt, false, now);
            }
        }
        let number = frame.picture.descriptor().frame;
        let reservation = self
            .reservations
            .iter()
            .position(|r| r.descriptor.frame == number)
            .ok_or(Error::InvalidMedia)?;
        let charge = self.reservations.remove(reservation).charge;
        // Native VideoFrame.close already succeeded. Drop the original compressed
        // receive allocation before returning the full reservation.
        drop(frame);
        let pressure = core.on_video_frame_closed(decoder, number)?;
        self.pressure(pressure)?;
        Ok(self.release_receipt(core, decoder, callback, number, charge))
    }
    pub fn decoder_closed(
        &mut self,
        core: &mut BrowserSessionCore,
        serial: u64,
    ) -> Result<Vec<ReleaseReceipt>, Error> {
        if self.quarantined {
            return Err(Error::MediaCleanup);
        }
        let decoder = self.decoder(serial)?;
        let mut released = Vec::with_capacity(self.frames.len());
        let mut i = 0;
        while i < self.frames.len() {
            if self.frames[i].decoder == decoder && !self.frames[i].escaped {
                let frame = self.frames.remove(i);
                let number = frame.picture.descriptor().frame;
                let position = self
                    .reservations
                    .iter()
                    .position(|r| r.descriptor.frame == number)
                    .ok_or(Error::InvalidMedia)?;
                let charge = self.reservations.remove(position).charge;
                released.push((frame.callback, number, charge));
                drop(frame);
            } else {
                i += 1;
            }
        }
        let records = core.on_decoder_closed(decoder)?;
        for record in records {
            self.pressure(Some(record))?;
        }
        Ok(released
            .into_iter()
            .map(|(callback, frame, charge)| {
                self.release_receipt(core, decoder, callback, frame, charge)
            })
            .collect())
    }
    pub fn reclaim(
        &mut self,
        core: &mut BrowserSessionCore,
        serial: u64,
        now: u64,
    ) -> Result<(), Error> {
        let decoder = self.decoder(serial)?;
        let _ = core.on_decoder_reclaimed(decoder, now);
        self.close(core);
        Ok(())
    }
    pub fn retire_media(&mut self, core: &mut BrowserSessionCore) {
        // The player's initial configure fences an older media owner, if any.
        // It cannot close a session that has not constructed its first decoder.
        if self.decoder.is_some() {
            self.close(core);
        }
    }
    pub fn quarantine(&mut self, core: &mut BrowserSessionCore) {
        self.quarantined = true;
        self.close(core);
    }
    pub fn close(&mut self, core: &mut BrowserSessionCore) {
        core.close();
        self.closed = true;
        if let Some(view) = self.view.as_mut() {
            view.close();
        }
        self.control.clear();
        self.control_taken = None;
        self.video_feedback.clear();
        if let Some(binding) = self.receiver_decoder.as_mut() {
            binding.revoke();
        }
        if let Some(receiver) = self.receiver.as_mut() {
            receiver.close();
        }
        // Queued allocations were actually dropped. Submitted work stays with
        // its original native decoder/output even after this connection closes.
        let _ = self.discard_queued(core);
    }
    pub fn snapshot(&self, core: &BrowserSessionCore) -> MediaSnapshot {
        let (retained_decoders, inflight_frames, inflight_bytes) = core.media_owner_counts();
        MediaSnapshot {
            active_decoder: if self.closed {
                None
            } else {
                core.active_decoder().map(|d| d.serial.to_string())
            },
            retained_decoders,
            inflight_frames,
            inflight_bytes,
            quarantined: self.quarantined,
            stage: if self.quarantined {
                "quarantined"
            } else if self.closed {
                "closed"
            } else {
                match core.snapshot().decoder_stage {
                    BrowserDecoderStage::AwaitingConfig => "awaiting_configuration",
                    BrowserDecoderStage::Configured { .. } => "configured",
                    BrowserDecoderStage::FirstFrameDecoded { .. } => "decoded",
                    BrowserDecoderStage::Presented { .. } => "submitted_to_canvas",
                    BrowserDecoderStage::NeedsRecovery { .. } => "needs_recovery",
                }
            },
        }
    }
}
