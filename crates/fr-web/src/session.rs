//! Browser coordination over the shared Rust session, clock and HEVC owners.
//! Authentication, observation, visible source freshness and input permission
//! are distinct. Every decoder callback and receive-credit release keeps its
//! original decoder instance and channel epoch, including during teardown.

use crate::{
    credit::{BrowserCreditError, BrowserCreditTracker},
    hevc::{
        BrowserDecoderStage, BrowserHevcError, BrowserHevcPipeline, DecodedFrameRelease,
        DecoderInstance, PreparedWebCodecsChunk, WebCodecsConfig,
    },
    protocol::BrowserTransportProfile,
};
use fr_client::{
    ClientSession, ReconnectPolicy, SessionError, SessionState, ShortcutCaptureController,
    clock::ClockExchange,
    input::ClientInstant,
    session::CloseReason,
    shortcut::{PlatformCapabilityRow, PlatformId},
};
use fr_core::{
    ids::{
        DisplayGeometryGeneration, InputLeaseId, InputTicketId, RemoteSessionId,
        ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_media::{
    config::CodecConfiguration,
    freshness::{ClockCorrelation, ClockPolicy},
};
use fr_transport::wss::{ChannelGeneration, ChannelSupervisor, WssChannelRole, WssError};
use fr_wire::{
    negotiation::ControlBinding,
    receiver_pressure::{ChannelRole, RECORD_BYTES as RECEIVER_PRESSURE_BYTES},
};
use serde::{Deserialize, Serialize};

/// Browser events are input fences, independent of socket liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserLifecycleEvent {
    TabHidden,
    TabVisible,
    FocusLost,
    FocusGained,
    PageHide,
    BfcacheRestore,
}

/// A host grant parsed on the original admitted control channel. Deadlines are
/// HOST monotonic timestamps, never durations starting at receipt. This local
/// check supplements the host's independent submission-time authority check.
#[derive(Debug, Clone, Copy)]
pub struct BrowserControlGrant {
    pub request_sequence: u64,
    pub lease: InputLeaseId,
    pub ticket: InputTicketId,
    pub lease_deadline_host_us: u64,
    pub ticket_deadline_host_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // Independent serialized UI facts, derived from the state machine.
pub struct BrowserSessionSnapshot {
    pub state_label: String,
    pub transport_profile: BrowserTransportProfile,
    pub transport_label: String,
    pub is_degraded_transport: bool,
    pub decoder_stage: BrowserDecoderStage,
    pub visible: bool,
    pub focused: bool,
    pub view_fresh: bool,
    pub can_request_control: bool,
    pub is_controlling: bool,
    pub video_generation: u64,
    pub inflight_frames: usize,
    pub inflight_bytes: u64,
    pub released_frames: u64,
    pub released_bytes: u64,
    pub browser_shortcut_mode: String,
}

struct ChannelCredit {
    generation: ChannelGeneration,
    tracker: BrowserCreditTracker,
}
/// One original frame reservation, admitted before reassembly allocation.
struct ReceivedCredit {
    channel: ChannelGeneration,
    frame: u64,
    charged: u64,
    decoder: Option<DecoderInstance>,
}
struct DecoderCredit {
    decoder: DecoderInstance,
    channel: ChannelGeneration,
    frames: usize,
}

/// One browser connection. Call `tick` from the monotonic browser timer; all
/// control operations also tick immediately before checking permission. Reuse
/// after closure requires disposal of every old decoder/output first.
pub struct BrowserSessionCore {
    session: ClientSession,
    shortcuts: ShortcutCaptureController,
    transport: BrowserTransportProfile,
    hevc: BrowserHevcPipeline,
    video_supervisor: ChannelSupervisor,
    credits: Vec<ChannelCredit>,
    decoder_credits: Vec<DecoderCredit>,
    received_credits: Vec<ReceivedCredit>,
    active_credit: Option<ChannelGeneration>,
    clock: Option<ClockExchange>,
    binding: Option<ControlBinding>,
    last_credit_epoch: u64,
    mapping: (DisplayGeometryGeneration, ViewportMappingGeneration),
    limits: ProtocolLimits,
    visible: bool,
    focused: bool,
    last_now_us: u64,
    last_admitted: Option<(u64, u64)>,
    presentation_floor: Option<(u64, u64)>,
    presented_source_us: Option<u64>,
    control_until_us: Option<u64>,
}

impl BrowserSessionCore {
    #[must_use]
    pub fn new(transport: BrowserTransportProfile, limits: ProtocolLimits) -> Self {
        Self {
            session: ClientSession::new(ReconnectPolicy::default()),
            shortcuts: ShortcutCaptureController::new(PlatformCapabilityRow::for_platform(
                PlatformId::Browser,
            )),
            transport,
            hevc: BrowserHevcPipeline::new(4, limits.per_viewer_compressed_bytes()),
            video_supervisor: ChannelSupervisor::new(WssChannelRole::Video, 4),
            credits: Vec::new(),
            decoder_credits: Vec::new(),
            received_credits: Vec::new(),
            active_credit: None,
            clock: None,
            binding: None,
            last_credit_epoch: 0,
            mapping: (
                DisplayGeometryGeneration::INITIAL,
                ViewportMappingGeneration::INITIAL,
            ),
            limits,
            visible: true,
            focused: true,
            last_now_us: 0,
            last_admitted: None,
            presentation_floor: None,
            presented_source_us: None,
            control_until_us: None,
        }
    }

    pub fn begin_connect(&mut self, now_us: u64) -> Result<(), SessionError> {
        if !matches!(
            self.session.state(),
            SessionState::Disconnected | SessionState::Closed { .. }
        ) || self.hevc.retained_decoders() != 0
            || self.hevc.inflight_frames() != 0
            || now_us < self.last_now_us
        {
            return Err(SessionError::InvalidState);
        }
        self.hevc
            .reset_session()
            .map_err(|_| SessionError::InvalidState)?;
        self.session.connect(ClientInstant(now_us))?;
        self.video_supervisor = ChannelSupervisor::new(WssChannelRole::Video, 4);
        self.credits.clear();
        self.decoder_credits.clear();
        self.received_credits.clear();
        self.active_credit = None;
        self.clock = None;
        self.binding = None;
        self.last_credit_epoch = 0;
        self.last_admitted = None;
        self.presentation_floor = None;
        self.presented_source_us = None;
        self.control_until_us = None;
        self.last_now_us = now_us;
        Ok(())
    }

    /// Retain the host's one original consent request without authorizing any
    /// observation or constructing media owners. The host timestamp is display
    /// metadata until clock correlation exists, never a client-clock timeout.
    pub(crate) fn on_waiting_approval(
        &mut self,
        request: RemoteSessionId,
        host_deadline_us: u64,
        now_us: u64,
    ) -> Result<(), SessionError> {
        if request.as_raw() == 0 || host_deadline_us == 0 || now_us < self.last_now_us {
            return Err(SessionError::InvalidState);
        }
        self.session
            .on_waiting_approval(request, host_deadline_us)?;
        self.last_now_us = now_us;
        Ok(())
    }

    /// Only an admitted browser session may disclose a display catalog.
    pub(crate) fn on_display_catalog(
        &mut self,
        catalog: &fr_wire::display::Catalog,
    ) -> Result<(), SessionError> {
        if self.binding.is_none() {
            return Err(SessionError::InvalidState);
        }
        self.session.on_catalog_received(*catalog)
    }

    pub(crate) fn select_display(&mut self, handle: u128) -> Result<(), SessionError> {
        if self.binding.is_none() {
            return Err(SessionError::InvalidState);
        }
        self.session.select_display(handle)
    }

    /// Install credit and clock owners atomically with host `SessionOpened`.
    /// Bootstrap transport authentication alone must never call this method.
    pub fn on_session_opened(
        &mut self,
        binding: ControlBinding,
        video_channel_binding: u32,
        credit_epoch: u64,
        now_us: u64,
    ) -> Result<(), BrowserCreditError> {
        let tracker = BrowserCreditTracker::new(
            binding,
            video_channel_binding,
            ChannelRole::Video,
            credit_epoch,
            self.limits,
        )?;
        self.on_admitted_session(binding, self.limits, now_us)?;
        let generation = self.active_video_generation();
        self.credits.push(ChannelCredit {
            generation,
            tracker,
        });
        self.active_credit = Some(generation);
        self.last_credit_epoch = credit_epoch;
        self.last_now_us = now_us;
        Ok(())
    }

    /// Install an authenticated, locally approved `SessionOpened` before its
    /// independently attached media channel. Transport admission is insufficient.
    pub(crate) fn on_admitted_session(
        &mut self,
        binding: ControlBinding,
        limits: ProtocolLimits,
        now_us: u64,
    ) -> Result<(), BrowserCreditError> {
        if self.binding.is_some()
            || self.hevc.retained_decoders() != 0
            || now_us < self.last_now_us
            || limits.negotiated(&self.limits) != limits
        {
            return Err(BrowserCreditError::InvalidBindingOrEpoch);
        }
        let clock = ClockExchange::new(
            binding,
            limits,
            ClockPolicy::default(),
            ClientInstant(now_us),
        )
        .map_err(|_| BrowserCreditError::InvalidBindingOrEpoch)?;
        self.session
            .on_session_opened(binding, ClientInstant(now_us))
            .map_err(|_| BrowserCreditError::InvalidBindingOrEpoch)?;
        self.limits = limits;
        self.hevc = BrowserHevcPipeline::new(4, limits.per_viewer_compressed_bytes());
        self.clock = Some(clock);
        self.binding = Some(binding);
        self.last_now_us = now_us;
        Ok(())
    }

    /// Begin the shared correlated clock protocol. Mark it queued only after
    /// these exact bytes enter the authenticated bounded control transport.
    pub fn begin_clock_probe(&mut self, now_us: u64) -> Result<Vec<u8>, fr_client::clock::Error> {
        let clock = self
            .clock
            .as_mut()
            .ok_or(fr_client::clock::Error::Configuration)?;
        clock.begin(ClientInstant(now_us))?;
        clock
            .pending(ClientInstant(now_us))?
            .map(|(bytes, _)| bytes.to_vec())
            .ok_or(fr_client::clock::Error::Expired)
    }
    pub fn clock_probe_queued(&mut self, now_us: u64) -> Result<(), fr_client::clock::Error> {
        self.clock
            .as_mut()
            .ok_or(fr_client::clock::Error::Configuration)?
            .queued(ClientInstant(now_us))
    }
    pub fn on_clock_reply(
        &mut self,
        bytes: &[u8],
        now_us: u64,
    ) -> Result<(), fr_client::clock::Error> {
        let result = self
            .clock
            .as_mut()
            .ok_or(fr_client::clock::Error::Configuration)?
            .accept(bytes, ClientInstant(now_us));
        if result
            .as_ref()
            .is_err_and(|error| *error != fr_client::clock::Error::LateReply)
        {
            self.fence_view();
        }
        result.map(|_| ())
    }

    fn observing(&self) -> bool {
        matches!(
            self.session.state(),
            SessionState::Viewing { .. }
                | SessionState::RequestingControl { .. }
                | SessionState::Controlling { .. }
                | SessionState::Suspended { .. }
        )
    }
    fn fence_view(&mut self) {
        self.session.invalidate_control();
        self.control_until_us = None;
        self.presented_source_us = None;
        self.presentation_floor = self.last_admitted;
    }
    pub(crate) fn correlation(&mut self, now_us: u64) -> Option<ClockCorrelation> {
        self.clock
            .as_mut()?
            .correlation(ClientInstant(now_us))
            .ok()
            .flatten()
    }

    /// Reevaluate source age with the full authenticated clock uncertainty.
    /// Socket traffic and delayed presentation never restart the freshness TTL.
    pub fn tick(&mut self, now_us: u64) -> Result<(), SessionError> {
        if now_us < self.last_now_us {
            self.close();
            return Err(SessionError::ArithmeticOverflow);
        }
        self.last_now_us = now_us;
        let source = self.presented_source_us;
        let correlation = self.correlation(now_us);
        let fresh = self.visible
            && self.focused
            && self.observing()
            && source.zip(correlation).is_some_and(|(source, clock)| {
                clock
                    .age_upper_us(source, now_us)
                    .is_ok_and(|age| age < fr_client::input::Policy::default().view_age_us)
            });
        if fresh {
            self.session
                .update_view_freshness(true, ClientInstant(now_us))?;
        } else {
            // Preserve the last source observation for later clock correlation,
            // but retire control permanently; fresh evidence is a new request.
            self.session.invalidate_control();
            self.control_until_us = None;
        }
        if self.control_until_us.is_some_and(|until| now_us >= until) {
            self.session.invalidate_control();
            self.control_until_us = None;
            if fresh {
                self.session
                    .update_view_freshness(true, ClientInstant(now_us))?;
            }
        }
        Ok(())
    }

    pub fn admit_decoder_configuration(
        &mut self,
        config: CodecConfiguration,
        max_decoded_pictures: u8,
        hvcc_bytes: &[u8],
    ) -> Result<WebCodecsConfig, BrowserHevcError> {
        if !self.observing() {
            return Err(BrowserHevcError::NotConfigured);
        }
        let channel = self.active_credit.ok_or(BrowserHevcError::NotConfigured)?;
        let result = self.hevc.admit_decoder_configuration(
            config,
            self.limits,
            max_decoded_pictures,
            hvcc_bytes,
        )?;
        self.fence_view();
        self.decoder_credits
            .retain(|owner| self.hevc.contains_decoder(owner.decoder));
        self.decoder_credits.push(DecoderCredit {
            decoder: result.decoder,
            channel,
            frames: 0,
        });
        Ok(result)
    }
    pub fn on_decoder_configured(
        &mut self,
        decoder: DecoderInstance,
    ) -> Result<(), BrowserHevcError> {
        self.hevc.on_decoder_configured(decoder)
    }

    /// A native decoder error belongs to the instance that registered its
    /// callback. Reject obsolete owners before observing their callback time or
    /// fencing view/control, so a delayed error cannot retire a replacement.
    pub fn on_decoder_reclaimed(
        &mut self,
        decoder: DecoderInstance,
        now_us: u64,
    ) -> Result<(), BrowserHevcError> {
        if self.hevc.active_decoder() != Some(decoder) || !self.hevc.contains_decoder(decoder) {
            return Err(BrowserHevcError::StaleGeneration);
        }
        if now_us < self.last_now_us {
            self.close();
            return Err(BrowserHevcError::StaleGeneration);
        }
        self.hevc.on_decoder_reclaimed(decoder)?;
        self.last_now_us = now_us;
        self.fence_view();
        Ok(())
    }
    pub fn active_decoder(&self) -> Option<DecoderInstance> {
        self.hevc.active_decoder()
    }

    pub fn admit_video_chunk(
        &mut self,
        channel_gen: ChannelGeneration,
        decoder: DecoderInstance,
        frame_id: u64,
        source_timestamp_us: u64,
        declared_idr: bool,
        length_prefixed_au: &[u8],
    ) -> Result<PreparedWebCodecsChunk, BrowserHevcError> {
        if !self.observing()
            || self.video_supervisor.check_admission(channel_gen).is_err()
            || self.active_credit != Some(channel_gen)
        {
            return Err(BrowserHevcError::StaleGeneration);
        }
        let credit = self
            .credits
            .iter_mut()
            .find(|owner| owner.generation == channel_gen)
            .ok_or(BrowserHevcError::NotConfigured)?;
        let decoder_owner = self
            .decoder_credits
            .iter_mut()
            .find(|owner| owner.decoder == decoder && owner.channel == channel_gen)
            .ok_or(BrowserHevcError::StaleGeneration)?;
        credit
            .tracker
            .can_record_received(length_prefixed_au.len() as u64)
            .map_err(BrowserHevcError::Credit)?;
        let chunk = self.hevc.admit_access_unit(
            decoder,
            frame_id,
            source_timestamp_us,
            declared_idr,
            length_prefixed_au,
        )?;
        // The preflight above and this mutation have one exclusive owner.
        credit
            .tracker
            .record_received(chunk.charged_bytes)
            .map_err(BrowserHevcError::Credit)?;
        decoder_owner.frames += 1;
        self.received_credits.push(ReceivedCredit {
            channel: channel_gen,
            frame: frame_id,
            charged: chunk.charged_bytes,
            decoder: Some(decoder),
        });
        self.last_admitted = Some((decoder.serial, frame_id));
        Ok(chunk)
    }
    /// Reserve the full declared picture, metadata and native-input copy before
    /// the receive pipeline allocates its first fragment. This is not decoding.
    pub(crate) fn reserve_video_frame(
        &mut self,
        channel: ChannelGeneration,
        frame: u64,
        charged: u64,
    ) -> Result<(), BrowserHevcError> {
        if !self.observing()
            || self.active_credit != Some(channel)
            || self.video_supervisor.check_admission(channel).is_err()
            || self
                .received_credits
                .iter()
                .any(|r| r.channel == channel && r.frame == frame)
            || self.received_credits.len() >= 4
            || self
                .received_credits
                .iter()
                .map(|r| r.charged)
                .sum::<u64>()
                .checked_add(charged)
                .is_none_or(|bytes| bytes > self.limits.per_viewer_compressed_bytes())
        {
            return Err(BrowserHevcError::DecodeQueueFull);
        }
        let tracker = &mut self
            .credits
            .iter_mut()
            .find(|owner| owner.generation == channel)
            .ok_or(BrowserHevcError::NotConfigured)?
            .tracker;
        tracker
            .record_received(charged)
            .map_err(BrowserHevcError::Credit)?;
        self.received_credits.push(ReceivedCredit {
            channel,
            frame,
            charged,
            decoder: None,
        });
        Ok(())
    }

    pub(crate) fn admit_reserved_video_chunk(
        &mut self,
        channel: ChannelGeneration,
        decoder: DecoderInstance,
        frame: u64,
        source: u64,
        declared_idr: bool,
        bytes: &[u8],
    ) -> Result<PreparedWebCodecsChunk, BrowserHevcError> {
        if !self.observing()
            || self.active_credit != Some(channel)
            || self.video_supervisor.check_admission(channel).is_err()
        {
            return Err(BrowserHevcError::StaleGeneration);
        }
        let reservation = self
            .received_credits
            .iter_mut()
            .find(|r| r.channel == channel && r.frame == frame && r.decoder.is_none())
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        if reservation.charged < bytes.len() as u64 {
            return Err(BrowserHevcError::DecodeQueueFull);
        }
        let owner = self
            .decoder_credits
            .iter_mut()
            .find(|r| r.channel == channel && r.decoder == decoder)
            .ok_or(BrowserHevcError::StaleGeneration)?;
        let chunk = self
            .hevc
            .admit_access_unit(decoder, frame, source, declared_idr, bytes)?;
        reservation.decoder = Some(decoder);
        owner.frames += 1;
        self.last_admitted = Some((decoder.serial, frame));
        Ok(chunk)
    }

    /// Called after the original reassembly allocation was positively discarded
    /// without submitting a decoder. JavaScript cannot forge this transition.
    pub(crate) fn discard_received_frame(
        &mut self,
        channel: ChannelGeneration,
        frame: u64,
    ) -> Result<Option<[u8; RECEIVER_PRESSURE_BYTES]>, BrowserHevcError> {
        let position = self
            .received_credits
            .iter()
            .position(|r| r.channel == channel && r.frame == frame && r.decoder.is_none())
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        let reservation = self.received_credits.remove(position);
        self.release_charge(channel, reservation.charged)
    }

    fn release_charge(
        &mut self,
        channel: ChannelGeneration,
        charged: u64,
    ) -> Result<Option<[u8; RECEIVER_PRESSURE_BYTES]>, BrowserHevcError> {
        let credit = self
            .credits
            .iter_mut()
            .find(|owner| owner.generation == channel)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        let record = credit
            .tracker
            .record_freed_and_encode(
                charged,
                credit.tracker.outstanding_bytes().saturating_sub(charged),
                u32::try_from(credit.tracker.outstanding_frames().saturating_sub(1))
                    .unwrap_or(u32::MAX),
            )
            .map_err(BrowserHevcError::Credit)?;
        self.credits.retain(|owner| {
            Some(owner.generation) == self.active_credit || owner.tracker.outstanding_frames() != 0
        });
        Ok(self.observing().then_some(record))
    }

    pub(crate) fn retain_video_output(
        &mut self,
        decoder: DecoderInstance,
        frame: u64,
    ) -> Result<(), BrowserHevcError> {
        self.hevc.retain_output(decoder, frame)
    }

    pub(crate) fn media_owner_counts(&self) -> (usize, usize, u64) {
        (
            self.hevc.retained_decoders(),
            self.hevc.inflight_frames(),
            self.hevc.inflight_bytes(),
        )
    }

    pub fn on_frame_decoded(
        &mut self,
        decoder: DecoderInstance,
        frame_id: u64,
    ) -> Result<(), BrowserHevcError> {
        self.hevc.on_frame_decoded(decoder, frame_id)
    }
    /// Only the canonical receiver-bound `ViewTracker` can supply this sample.
    /// No sample means the rendered image remains unqualified, even when its
    /// host capture timestamp alone falls inside the source-age bound.
    pub(crate) fn apply_view_sample(
        &mut self,
        sample: Option<fr_wire::presented::Sample>,
        now_us: u64,
    ) -> Result<(), SessionError> {
        self.presented_source_us = None;
        if let (
            Some(sample),
            BrowserDecoderStage::Presented {
                last_frame_id,
                last_source_us,
                ..
            },
        ) = (sample, self.hevc.stage())
            && sample.stamp.frame == last_frame_id
            && sample.stamp.captured_us == last_source_us
            && sample.stamp.observed_us >= last_source_us
            && sample.stamp.source != fr_wire::SourceObservation::Unknown
        {
            self.presented_source_us = Some(sample.stamp.observed_us);
        }
        if self.presented_source_us.is_none() {
            self.session.invalidate_control();
            self.control_until_us = None;
        }
        self.tick(now_us)
    }

    pub fn on_frame_presented(
        &mut self,
        decoder: DecoderInstance,
        frame_id: u64,
        now_us: u64,
    ) -> Result<(), BrowserHevcError> {
        if self.hevc.active_decoder() != Some(decoder)
            || !self.hevc.contains_decoder(decoder)
            || !self.observing()
            || self
                .presentation_floor
                .is_some_and(|floor| (decoder.serial, frame_id) <= floor)
        {
            return Err(BrowserHevcError::StaleGeneration);
        }
        if now_us < self.last_now_us {
            self.close();
            return Err(BrowserHevcError::StaleGeneration);
        }
        self.hevc
            .on_frame_presented(decoder, frame_id, now_us, self.visible)?;
        if let BrowserDecoderStage::Presented { last_source_us, .. } = self.hevc.stage() {
            self.presented_source_us = Some(last_source_us);
        }
        // Presentation is an observed rendering event. Freshness remains false
        // if source age is unknown, old, or from an expired clock correlation.
        self.tick(now_us)
            .map_err(|_| BrowserHevcError::StaleGeneration)
    }

    fn release_credit(
        &mut self,
        release: DecodedFrameRelease,
    ) -> Result<Option<[u8; RECEIVER_PRESSURE_BYTES]>, BrowserHevcError> {
        let decoder = self
            .decoder_credits
            .iter_mut()
            .find(|owner| owner.decoder == release.decoder)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        let position = self
            .received_credits
            .iter()
            .position(|r| {
                r.channel == decoder.channel
                    && r.frame == release.frame_id
                    && r.decoder == Some(release.decoder)
            })
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        let reservation = self.received_credits.remove(position);
        decoder.frames = decoder
            .frames
            .checked_sub(1)
            .ok_or(BrowserHevcError::UnknownFrameId)?;
        let record = self.release_charge(reservation.channel, reservation.charged)?;
        self.decoder_credits
            .retain(|owner| owner.frames != 0 || self.hevc.contains_decoder(owner.decoder));
        Ok(record)
    }
    pub fn on_video_frame_closed(
        &mut self,
        decoder: DecoderInstance,
        frame_id: u64,
    ) -> Result<Option<[u8; RECEIVER_PRESSURE_BYTES]>, BrowserHevcError> {
        let release = self.hevc.on_video_frame_closed(decoder, frame_id)?;
        self.release_credit(release)
    }
    /// Confirm decoder.close/reset has actually completed. Outstanding decoded
    /// `VideoFrame` outputs still require their individual close callbacks.
    pub fn on_decoder_closed(
        &mut self,
        decoder: DecoderInstance,
    ) -> Result<Vec<[u8; RECEIVER_PRESSURE_BYTES]>, BrowserHevcError> {
        let active = self.hevc.active_decoder() == Some(decoder);
        let releases = self.hevc.on_decoder_closed(decoder)?;
        if active {
            self.fence_view();
        }
        let mut records = Vec::with_capacity(releases.len());
        for release in releases {
            if let Some(record) = self.release_credit(release)? {
                records.push(record);
            }
        }
        self.decoder_credits
            .retain(|owner| owner.frames != 0 || self.hevc.contains_decoder(owner.decoder));
        Ok(records)
    }

    pub fn request_control(&mut self, now_us: u64) -> Result<u64, SessionError> {
        self.tick(now_us)?;
        if !self.visible || !self.focused {
            return Err(SessionError::InputSuspended);
        }
        self.session.request_control(ClientInstant(now_us))
    }
    pub fn on_control_granted(
        &mut self,
        grant: BrowserControlGrant,
        now_us: u64,
    ) -> Result<(), SessionError> {
        if !matches!(self.state(), SessionState::RequestingControl { request_sequence, .. } if *request_sequence == grant.request_sequence)
        {
            return Err(SessionError::InvalidState);
        }
        self.tick(now_us)?;
        if !matches!(self.state(), SessionState::RequestingControl { request_sequence, .. } if *request_sequence == grant.request_sequence)
        {
            return Err(SessionError::InvalidState);
        }
        let clock = self.correlation(now_us).ok_or(SessionError::ViewNotFresh)?;
        if grant.lease.as_raw() == 0
            || grant.ticket.as_raw() == 0
            || grant.ticket_deadline_host_us > grant.lease_deadline_host_us
        {
            return Err(SessionError::LeaseRevoked);
        }
        let until = clock
            .deadline_lower_us(grant.ticket_deadline_host_us, now_us)
            .map_err(|_| SessionError::LeaseRevoked)?;
        if now_us >= until {
            return Err(SessionError::LeaseRevoked);
        }
        self.session
            .on_control_granted(grant.request_sequence, grant.lease, grant.ticket)?;
        self.control_until_us = Some(until);
        Ok(())
    }
    pub fn authorize_input(
        &mut self,
        display_gen: DisplayGeometryGeneration,
        viewport_gen: ViewportMappingGeneration,
        now_us: u64,
    ) -> Result<(), SessionError> {
        self.tick(now_us)?;
        if !self.visible
            || !self.focused
            || self.control_until_us.is_none_or(|until| now_us >= until)
        {
            return Err(SessionError::InputSuspended);
        }
        self.session
            .validate_coordinate_mapping(display_gen, viewport_gen)
    }
    pub fn update_mapping(
        &mut self,
        display_gen: DisplayGeometryGeneration,
        viewport_gen: ViewportMappingGeneration,
    ) {
        if self.mapping != (display_gen, viewport_gen) {
            self.fence_view();
            self.mapping = (display_gen, viewport_gen);
        }
        self.session.update_mapping(display_gen, viewport_gen);
    }

    pub fn on_lifecycle_event(&mut self, event: BrowserLifecycleEvent, now_us: u64) {
        if now_us < self.last_now_us {
            self.close();
            return;
        }
        self.last_now_us = now_us;
        self.fence_view();
        match event {
            BrowserLifecycleEvent::TabHidden => {
                self.visible = false;
            }
            BrowserLifecycleEvent::TabVisible => {
                self.visible = true;
            }
            BrowserLifecycleEvent::FocusLost => {
                self.focused = false;
            }
            BrowserLifecycleEvent::FocusGained => {
                self.focused = true;
            }
            BrowserLifecycleEvent::PageHide | BrowserLifecycleEvent::BfcacheRestore => {
                self.visible = false;
                self.close();
            }
        }
        self.session.set_window_visible(self.visible);
    }
    pub fn close(&mut self) {
        self.fence_view();
        self.hevc.fence_active_decoder();
        self.active_credit = None;
        if let Some(clock) = self.clock.as_mut() {
            clock.stop();
        }
        self.session.close(CloseReason::UserRequested);
    }

    /// Replacement fences old decode/presentation and input first. A new
    /// attachment must install a strictly newer credit epoch on this channel
    /// generation before use. A compact binding may repeat only under that new
    /// epoch; queued releases retain their original epoch and channel owner.
    pub fn replace_video_channel(&mut self) -> Result<ChannelGeneration, WssError> {
        let generation = self.video_supervisor.replace_channel()?;
        self.fence_view();
        self.hevc.fence_active_decoder();
        self.active_credit = None;
        self.credits
            .retain(|owner| owner.tracker.outstanding_frames() != 0);
        Ok(generation)
    }
    pub fn attach_video_channel(
        &mut self,
        binding: ControlBinding,
        channel_binding: u32,
        credit_epoch: u64,
    ) -> Result<(), BrowserCreditError> {
        if !self.observing()
            || self.active_credit.is_some()
            || credit_epoch <= self.last_credit_epoch
        {
            return Err(BrowserCreditError::InvalidBindingOrEpoch);
        }
        // Replacing a channel never replaces its parent session authority.
        if self.binding != Some(binding) {
            return Err(BrowserCreditError::InvalidBindingOrEpoch);
        }
        let tracker = BrowserCreditTracker::new(
            binding,
            channel_binding,
            ChannelRole::Video,
            credit_epoch,
            self.limits,
        )?;
        let generation = self.active_video_generation();
        self.credits.push(ChannelCredit {
            generation,
            tracker,
        });
        self.active_credit = Some(generation);
        self.last_credit_epoch = credit_epoch;
        Ok(())
    }
    pub fn complete_video_channel_close(&mut self, generation: ChannelGeneration) {
        self.video_supervisor.complete_close(generation);
    }
    pub fn active_video_generation(&self) -> ChannelGeneration {
        self.video_supervisor.active_generation()
    }
    pub fn state(&self) -> &SessionState {
        self.session.state()
    }
    pub fn shortcuts(&self) -> &ShortcutCaptureController {
        &self.shortcuts
    }

    /// Last checked state; call tick with current monotonic time before rendering.
    #[must_use]
    pub fn snapshot(&self) -> BrowserSessionSnapshot {
        let diag = self.session.diagnostics();
        let (released_frames, released_bytes) = self
            .credits
            .iter()
            .find(|owner| Some(owner.generation) == self.active_credit)
            .map_or((0, 0), |owner| {
                (
                    owner.tracker.released_frames(),
                    owner.tracker.released_bytes(),
                )
            });
        BrowserSessionSnapshot {
            state_label: diag.state_label.to_owned(),
            transport_profile: self.transport,
            transport_label: self.transport.diagnostic_label().to_owned(),
            is_degraded_transport: self.transport.is_degraded(),
            decoder_stage: self.hevc.stage(),
            visible: self.visible,
            focused: self.focused,
            view_fresh: diag.view_fresh,
            can_request_control: self.visible && self.focused && self.session.can_request_control(),
            is_controlling: diag.is_controlling,
            video_generation: self.active_video_generation().0,
            inflight_frames: self.hevc.inflight_frames(),
            inflight_bytes: self.hevc.inflight_bytes(),
            released_frames,
            released_bytes,
            browser_shortcut_mode: self.shortcuts.mode().label().to_owned(),
        }
    }
}
