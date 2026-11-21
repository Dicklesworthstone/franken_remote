//! Unified browser workstation session core over `fr-client`, `fr-media`, and `fr-transport`.
//!
//! Coordinates:
//! - `fr_client::ClientSession` (strict observation/readiness/control separation)
//! - `fr_client::ShortcutCaptureController` (`PlatformId::Browser` reserved-shortcut truth)
//! - `BrowserHevcPipeline` (non-circular WebCodecs startup + `HevcGuard`)
//! - `BrowserCreditTracker` (cumulative `0x0081 ReceiverPressure` on `VideoFrame.close()`)
//! - `fr_transport::wss::ChannelSupervisor` (generation fencing across channel resets)

use crate::{
    credit::{BrowserCreditError, BrowserCreditTracker},
    hevc::{
        BrowserDecoderStage, BrowserHevcError, BrowserHevcPipeline, PreparedWebCodecsChunk,
        WebCodecsConfig,
    },
    protocol::BrowserTransportProfile,
};
use fr_client::{
    ClientSession, ReconnectPolicy, SessionError, SessionState, ShortcutCaptureController,
    input::ClientInstant,
    shortcut::{PlatformCapabilityRow, PlatformId},
};
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        ViewportMappingGeneration,
    },
    limits::ProtocolLimits,
};
use fr_media::config::CodecConfiguration;
use fr_transport::wss::{ChannelGeneration, ChannelSupervisor, WssChannelRole, WssError};
use fr_wire::{
    negotiation::ControlBinding,
    receiver_pressure::{ChannelRole, RECORD_BYTES as RECEIVER_PRESSURE_BYTES},
};
use serde::{Deserialize, Serialize};

/// Browser DOM / page lifecycle events that MUST fence input authority immediately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BrowserLifecycleEvent {
    /// `document.visibilitychange` transitioned to hidden.
    TabHidden,
    /// `document.visibilitychange` transitioned back to visible (still requires fresh frame!).
    TabVisible,
    /// Window blur / focus loss.
    FocusLost,
    /// `pagehide` event (navigation away or tab discard).
    PageHide,
    /// `pageshow` with `event.persisted === true` (bfcache restoration).
    BfcacheRestore,
    /// Browser reclaimed the WebCodecs `VideoDecoder` hardware context.
    DecoderReclaimed,
}

/// Serializable status snapshot exposed to the thin browser UI shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserSessionSnapshot {
    pub state_label: String,
    pub transport_profile: BrowserTransportProfile,
    pub transport_label: String,
    pub is_degraded_transport: bool,
    pub decoder_stage: BrowserDecoderStage,
    pub visible: bool,
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

/// Complete browser workstation client core compiled cleanly for `wasm32` and host tests.
pub struct BrowserSessionCore {
    session: ClientSession,
    shortcuts: ShortcutCaptureController,
    transport: BrowserTransportProfile,
    hevc: BrowserHevcPipeline,
    video_supervisor: ChannelSupervisor,
    credit: Option<BrowserCreditTracker>,
    limits: ProtocolLimits,
    visible: bool,
}

impl BrowserSessionCore {
    /// Create a new disconnected browser session core.
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
            credit: None,
            limits,
            visible: true,
        }
    }

    /// Begin connecting to the host over the authenticated same-origin transport.
    pub fn begin_connect(&mut self, now_us: u64) -> Result<(), SessionError> {
        self.session.connect(ClientInstant(now_us))
    }

    /// Enter observation-only `Viewing` after host `SessionOpened` (never grants control!).
    pub fn on_session_opened(
        &mut self,
        binding: ControlBinding,
        video_channel_binding: u32,
        credit_epoch: u64,
        now_us: u64,
    ) -> Result<(), BrowserCreditError> {
        self.session
            .on_session_opened(binding, ClientInstant(now_us))
            .map_err(|_| BrowserCreditError::InvalidBindingOrEpoch)?;
        self.credit = Some(BrowserCreditTracker::new(
            binding,
            video_channel_binding,
            ChannelRole::Video,
            credit_epoch,
            self.limits,
        )?);
        Ok(())
    }

    /// Admit a host `DecoderConfiguration` `hvcC` record and return the exact WebCodecs config.
    pub fn admit_decoder_configuration(
        &mut self,
        config: CodecConfiguration,
        max_decoded_pictures: u8,
        hvcc_bytes: &[u8],
    ) -> Result<WebCodecsConfig, BrowserHevcError> {
        self.hevc
            .admit_decoder_configuration(config, self.limits, max_decoded_pictures, hvcc_bytes)
    }

    /// Confirm `VideoDecoder.configure()` succeeded (distinct from first decoded/presented frame).
    pub fn on_decoder_configured(
        &mut self,
        generation: CodecConfigurationGeneration,
    ) -> Result<(), BrowserHevcError> {
        self.hevc.on_decoder_configured(generation)
    }

    /// Admit an incoming video access unit on `channel_gen`, checking generation fencing,
    /// `HevcGuard` parameter/reference integrity, and bounded queue ceilings.
    pub fn admit_video_chunk(
        &mut self,
        channel_gen: ChannelGeneration,
        codec_gen: CodecConfigurationGeneration,
        frame_id: u64,
        source_timestamp_us: u64,
        declared_idr: bool,
        length_prefixed_au: &[u8],
    ) -> Result<PreparedWebCodecsChunk, BrowserHevcError> {
        if self.video_supervisor.check_admission(channel_gen).is_err() {
            return Err(BrowserHevcError::StaleGeneration);
        }
        let chunk = self.hevc.admit_access_unit(
            codec_gen,
            frame_id,
            source_timestamp_us,
            declared_idr,
            length_prefixed_au,
        )?;
        if let Some(credit) = self.credit.as_mut() {
            let _ = credit.record_received(chunk.charged_bytes);
        }
        Ok(chunk)
    }

    /// Record WebCodecs decode completion. Does NOT grant control and does NOT release credit.
    pub fn on_frame_decoded(
        &mut self,
        generation: CodecConfigurationGeneration,
        frame_id: u64,
    ) -> Result<(), BrowserHevcError> {
        self.hevc.on_frame_decoded(generation, frame_id)
    }

    /// Record visible presentation on canvas. Updates `ClientSession` view freshness ONLY when visible;
    /// still NEVER grants control or releases credit before `VideoFrame.close()`.
    pub fn on_frame_presented(
        &mut self,
        generation: CodecConfigurationGeneration,
        frame_id: u64,
        now_us: u64,
    ) -> Result<(), BrowserHevcError> {
        self.hevc
            .on_frame_presented(generation, frame_id, now_us, self.visible)?;
        let _ = self
            .session
            .update_view_freshness(true, ClientInstant(now_us));
        Ok(())
    }

    /// Record `VideoFrame.close()` completion and emit the encoded `0x0081 ReceiverPressure` record.
    pub fn on_video_frame_closed(
        &mut self,
        frame_id: u64,
    ) -> Result<Option<[u8; RECEIVER_PRESSURE_BYTES]>, BrowserHevcError> {
        let release = self.hevc.on_video_frame_closed(frame_id)?;
        let Some(credit) = self.credit.as_mut() else {
            return Ok(None);
        };
        let queued_items = u32::try_from(release.remaining_inflight_frames).unwrap_or(u32::MAX);
        let record = credit
            .record_freed_and_encode(
                release.freed_bytes,
                release.remaining_inflight_bytes,
                queued_items,
            )
            .map_err(|_| BrowserHevcError::UnknownFrameId)?;
        Ok(Some(record))
    }

    /// Explicitly request input control (only succeeds after visible fresh presentation!).
    pub fn request_control(&mut self, now_us: u64) -> Result<u64, SessionError> {
        if !self.visible {
            return Err(SessionError::InputSuspended);
        }
        self.session.request_control(ClientInstant(now_us))
    }

    /// Record host-granted control lease and validity ticket.
    pub fn on_control_granted(
        &mut self,
        lease: InputLeaseId,
        ticket: InputTicketId,
    ) -> Result<(), SessionError> {
        self.session.on_control_granted(lease, ticket)
    }

    /// Validate that pointer/key input is currently permitted for the given display/viewport generations.
    pub fn authorize_input(
        &mut self,
        display_gen: DisplayGeometryGeneration,
        viewport_gen: ViewportMappingGeneration,
    ) -> Result<(), SessionError> {
        if !self.visible {
            return Err(SessionError::InputSuspended);
        }
        self.session
            .validate_coordinate_mapping(display_gen, viewport_gen)
    }

    /// Update active display/viewport generations after geometry confirmation.
    pub fn update_mapping(
        &mut self,
        display_gen: DisplayGeometryGeneration,
        viewport_gen: ViewportMappingGeneration,
    ) {
        self.session.update_mapping(display_gen, viewport_gen);
    }

    /// Handle browser lifecycle events: hidden tabs, pagehide, bfcache restore, or decoder
    /// reclamation immediately suspend/invalidate control and require fresh view evidence.
    pub fn on_lifecycle_event(&mut self, event: BrowserLifecycleEvent, now_us: u64) {
        match event {
            BrowserLifecycleEvent::TabHidden => {
                self.visible = false;
                self.session.set_window_visible(false);
                let _ = self
                    .session
                    .update_view_freshness(false, ClientInstant(now_us));
            }
            BrowserLifecycleEvent::TabVisible => {
                self.visible = true;
                // Becoming visible again does NOT restore freshness until a fresh frame presents!
                self.session.set_window_visible(true);
            }
            BrowserLifecycleEvent::FocusLost => {
                self.session.on_focus_loss();
            }
            BrowserLifecycleEvent::PageHide | BrowserLifecycleEvent::BfcacheRestore => {
                self.visible = false;
                self.session.set_window_visible(false);
                let _ = self
                    .session
                    .update_view_freshness(false, ClientInstant(now_us));
                self.hevc.on_decoder_reclaimed();
            }
            BrowserLifecycleEvent::DecoderReclaimed => {
                self.hevc.on_decoder_reclaimed();
                let _ = self
                    .session
                    .update_view_freshness(false, ClientInstant(now_us));
            }
        }
    }

    /// Replace the video WSS channel with a newer generation, immediately fencing the old one.
    pub fn replace_video_channel(&mut self) -> Result<ChannelGeneration, WssError> {
        self.video_supervisor.replace_channel()
    }

    /// Complete teardown of a previously fenced video channel generation.
    pub fn complete_video_channel_close(&mut self, generation: ChannelGeneration) {
        self.video_supervisor.complete_close(generation);
    }

    /// Current active video channel generation.
    #[must_use]
    pub const fn active_video_generation(&self) -> ChannelGeneration {
        self.video_supervisor.active_generation()
    }

    /// Current underlying `ClientSession` state.
    #[must_use]
    pub fn state(&self) -> &SessionState {
        self.session.state()
    }

    /// Current shortcut capture controller exposing honest browser-reserved-shortcut rows.
    #[must_use]
    pub const fn shortcuts(&self) -> &ShortcutCaptureController {
        &self.shortcuts
    }

    /// Build a serializable UI/diagnostic snapshot.
    #[must_use]
    pub fn snapshot(&self) -> BrowserSessionSnapshot {
        let diag = self.session.diagnostics();
        let (released_frames, released_bytes) = self
            .credit
            .as_ref()
            .map_or((0, 0), |c| (c.released_frames(), c.released_bytes()));
        let shortcut_mode = self.shortcuts.mode().label();
        BrowserSessionSnapshot {
            state_label: diag.state_label.to_owned(),
            transport_profile: self.transport,
            transport_label: self.transport.diagnostic_label().to_owned(),
            is_degraded_transport: self.transport.is_degraded(),
            decoder_stage: self.hevc.stage(),
            visible: self.visible,
            view_fresh: diag.view_fresh,
            can_request_control: self.visible && self.session.can_request_control(),
            is_controlling: diag.is_controlling,
            video_generation: self.video_supervisor.active_generation().0,
            inflight_frames: self.hevc.inflight_frames(),
            inflight_bytes: self.hevc.inflight_bytes(),
            released_frames,
            released_bytes,
            browser_shortcut_mode: shortcut_mode.to_owned(),
        }
    }
}
