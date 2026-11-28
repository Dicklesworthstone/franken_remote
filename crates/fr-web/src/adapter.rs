//! Browser transport startup owned by Rust, before any desktop authority exists.
//!
//! One adapter is retained across connection attempts. It never stores a nonce
//! after hello issuance or retirement, never accepts a JavaScript-created
//! session binding, and delegates lifecycle fencing to [`BrowserSessionCore`].
//! The selected view-only WSS profile composes the shared session/HEVC/receive
//! owners. Observation, actual decoder readiness and visible freshness remain
//! distinct; this adapter exposes no remote input grant.

use crate::{BrowserLifecycleEvent, BrowserSessionCore, BrowserTransportProfile};
use core::fmt;
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    Kind,
    input::InputDelivery,
    negotiation::{Role, browser},
    refusal::{self, Reason},
};
use serde::{Deserialize, Serialize};

pub const BROWSER_ABI_VERSION: u32 = 2;
pub const MAX_BOOTSTRAP_BYTES: usize = 4096;
pub const MAX_CONTROL_RECORD_BYTES: usize = browser::MAX_RECORD;
const MAX_BOOTSTRAP_LIFETIME_MS: u32 = 15_000;
const TRANSPORT: BrowserTransportProfile = BrowserTransportProfile::WssDegraded {
    multiplexed: false,
    tcp_head_of_line: true,
};

/// Non-reused identity within one retained adapter. Zero is never allocated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrowserAttempt(pub(crate) u32);

impl BrowserAttempt {
    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserAdapterError {
    Cancelled,
    InvalidRole,
    InvalidBootstrap,
    UnsupportedProfile,
    DesktopUnavailable,
    InvalidState,
    Expired,
    ApprovalExpired,
    InvalidClock,
    InvalidControl,
    UnsupportedControl,
    MessageTooLarge,
    InvalidLifecycle,
    AttemptExhausted,
    Encoding,
    InvalidMedia,
    MediaCleanup,
    QueueFull,
}

impl BrowserAdapterError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Cancelled => "browser_connection_cancelled",
            Self::InvalidRole => "browser_role_invalid",
            Self::InvalidBootstrap => "browser_bootstrap_invalid",
            Self::UnsupportedProfile => "browser_transport_profile_unsupported",
            Self::DesktopUnavailable => "browser_session_unavailable",
            Self::InvalidState => "browser_connection_state_invalid",
            Self::Expired => "browser_handshake_expired",
            Self::ApprovalExpired => "browser_approval_expired",
            Self::InvalidClock => "browser_clock_invalid",
            Self::InvalidControl => "browser_control_record_invalid",
            Self::UnsupportedControl => "browser_control_record_unsupported",
            Self::MessageTooLarge => "browser_message_too_large",
            Self::InvalidLifecycle => "browser_lifecycle_invalid",
            Self::AttemptExhausted => "browser_attempts_exhausted",
            Self::Encoding => "browser_hello_encoding_failed",
            Self::InvalidMedia => "browser_media_record_invalid",
            Self::MediaCleanup => "browser_media_cleanup_incomplete",
            Self::QueueFull => "browser_media_capacity",
        }
    }
}

impl fmt::Display for BrowserAdapterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl core::error::Error for BrowserAdapterError {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Disconnected,
    Bootstrapping,
    AwaitingTransport,
    AwaitingHost,
    AwaitingApproval,
    Viewing,
    Refused,
    Closed,
}

impl Phase {
    const fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Bootstrapping => "bootstrapping",
            Self::AwaitingTransport => "awaiting_transport",
            Self::AwaitingHost => "awaiting_host",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Viewing => "viewing",
            Self::Refused => "refused",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum BootstrapRole {
    Observer,
    Controller,
}

impl BootstrapRole {
    fn parse(role: &str) -> Result<Self, BrowserAdapterError> {
        match role {
            "observer" => Ok(Self::Observer),
            "controller" => Ok(Self::Controller),
            _ => Err(BrowserAdapterError::InvalidRole),
        }
    }

    const fn wire(self) -> Role {
        match self {
            Self::Observer => Role::Observe,
            Self::Controller => Role::RequestControl,
        }
    }

    const fn request(self) -> &'static str {
        match self {
            Self::Observer => r#"{"role":"observer"}"#,
            Self::Controller => r#"{"role":"controller"}"#,
        }
    }
}

/// Borrow bounded JSON; never retain or report peer-controlled text.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapResponse<'a> {
    nonce_hex: Option<&'a str>,
    expires_in_ms: Option<u32>,
    transport_profile: Option<&'a str>,
    profile_id: Option<u8>,
    profile_version: Option<u16>,
    role: Option<BootstrapRole>,
    desktop_available: Option<bool>,
    refusal_code: Option<&'a str>,
}

/// Content-free DOM projection, with authority facts derived from the core.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[allow(clippy::struct_excessive_bools)] // Independent UI facts, not authority inputs.
pub struct BrowserAdapterSnapshot {
    pub attempt: Option<u32>,
    pub state_label: &'static str,
    pub transport_label: &'static str,
    pub visible: bool,
    pub focused: bool,
    pub view_fresh: bool,
    pub can_request_control: bool,
    pub is_controlling: bool,
    pub refusal_code: Option<&'static str>,
}

/// Owns the connection's nonce, bounded handshake, and shared session core.
/// There is deliberately no setter for a successful session or input grant.
pub struct BrowserConnectionAdapter {
    core: BrowserSessionCore,
    desktop: Option<crate::desktop::DesktopSession>,
    last_attempt: u32,
    attempt: Option<BrowserAttempt>,
    phase: Phase,
    role: BootstrapRole,
    nonce: Option<[u8; browser::NONCE_BYTES]>,
    started_us: u64,
    deadline_us: u64,
    last_now_us: u64,
    refusal_code: Option<&'static str>,
}

impl Default for BrowserConnectionAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserConnectionAdapter {
    pub fn new() -> Self {
        Self {
            core: BrowserSessionCore::new(TRANSPORT, ProtocolLimits::ABSOLUTE),
            desktop: None,
            last_attempt: 0,
            attempt: None,
            phase: Phase::Disconnected,
            role: BootstrapRole::Observer,
            nonce: None,
            started_us: 0,
            deadline_us: 0,
            last_now_us: 0,
            refusal_code: None,
        }
    }

    /// Starting a new attempt fences all callbacks from its predecessor.
    pub fn begin(
        &mut self,
        role: &str,
        now_us: u64,
    ) -> Result<BrowserAttempt, BrowserAdapterError> {
        let role = BootstrapRole::parse(role)?;
        if now_us < self.last_now_us {
            return self.fail(BrowserAdapterError::InvalidClock);
        }
        let Some(serial) = self.last_attempt.checked_add(1) else {
            return self.fail(BrowserAdapterError::AttemptExhausted);
        };
        let Some(deadline_us) = now_us
            .checked_add(u64::from(ProtocolLimits::ABSOLUTE.max_handshake_duration_ms()) * 1000)
        else {
            return self.fail(BrowserAdapterError::InvalidClock);
        };
        self.retire(Phase::Closed, None);
        if self.core.begin_connect(now_us).is_err() {
            return self.fail(BrowserAdapterError::InvalidState);
        }
        self.desktop = None;
        let attempt = BrowserAttempt(serial);
        self.last_attempt = serial;
        self.attempt = Some(attempt);
        self.phase = Phase::Bootstrapping;
        self.role = role;
        self.started_us = now_us;
        self.deadline_us = deadline_us;
        self.last_now_us = now_us;
        Ok(attempt)
    }

    pub fn bootstrap_request(
        &self,
        attempt: BrowserAttempt,
    ) -> Result<&'static str, BrowserAdapterError> {
        self.require_attempt(attempt)?;
        if self.phase != Phase::Bootstrapping {
            return Err(BrowserAdapterError::InvalidState);
        }
        Ok(self.role.request())
    }

    /// Capability refusal precedes nonce parsing or storage. Expiry is measured
    /// from the original request, never extended by a delayed response body.
    pub fn accept_bootstrap(
        &mut self,
        attempt: BrowserAttempt,
        body: &str,
        now_us: u64,
    ) -> Result<(), BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        if self.phase != Phase::Bootstrapping {
            return self.fail(BrowserAdapterError::InvalidState);
        }
        if body.len() > MAX_BOOTSTRAP_BYTES {
            return self.fail(BrowserAdapterError::MessageTooLarge);
        }
        let Ok(response) = serde_json::from_str::<BootstrapResponse<'_>>(body) else {
            return self.fail(BrowserAdapterError::InvalidBootstrap);
        };
        if response.desktop_available != Some(true) {
            return self.fail(BrowserAdapterError::DesktopUnavailable);
        }
        if response.profile_id != Some(browser::WSS_PROFILE)
            || response.profile_version != Some(browser::PROFILE_VERSION)
            || response.transport_profile != Some(TRANSPORT.diagnostic_label())
        {
            return self.fail(BrowserAdapterError::UnsupportedProfile);
        }
        if response.role != Some(self.role) || response.refusal_code.is_some() {
            return self.fail(BrowserAdapterError::InvalidBootstrap);
        }
        let Some(lifetime_ms) = response
            .expires_in_ms
            .filter(|&value| value > 0 && value <= MAX_BOOTSTRAP_LIFETIME_MS)
        else {
            return self.fail(BrowserAdapterError::InvalidBootstrap);
        };
        let Some(until_us) = self.started_us.checked_add(u64::from(lifetime_ms) * 1000) else {
            return self.fail(BrowserAdapterError::InvalidClock);
        };
        self.deadline_us = self.deadline_us.min(until_us);
        if now_us >= self.deadline_us {
            return self.fail(BrowserAdapterError::Expired);
        }
        let Some(nonce) = response.nonce_hex.and_then(parse_nonce) else {
            return self.fail(BrowserAdapterError::InvalidBootstrap);
        };
        self.nonce = Some(nonce);
        self.phase = Phase::AwaitingTransport;
        Ok(())
    }

    /// Issue exactly one canonical first reliable message. A failed transport
    /// send requires a new bootstrap; this nonce cannot be issued again.
    pub fn client_hello(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<Vec<u8>, BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        if self.phase != Phase::AwaitingTransport {
            return self.fail(BrowserAdapterError::InvalidState);
        }
        let Some(nonce) = self.nonce.take() else {
            return self.fail(BrowserAdapterError::InvalidState);
        };
        let mut offer = browser::offer(ProtocolLimits::ABSOLUTE);
        offer.role = self.role.wire();
        self.desktop = Some(crate::desktop::DesktopSession::new(offer.clone()));
        let Ok(hello) = browser::Hello::new(offer, nonce) else {
            return self.fail(BrowserAdapterError::Encoding);
        };
        let mut out = [0; MAX_CONTROL_RECORD_BYTES];
        let Ok(length) = browser::encode_hello(&hello, MAX_CONTROL_RECORD_BYTES, &mut out) else {
            return self.fail(BrowserAdapterError::Encoding);
        };
        // The one-use nonce has been consumed by this hello. The host still
        // checks its own original nonce expiry when it receives these bytes.
        // Negotiation retains the attempt's original ten-second ceiling.
        let Some(until) = self
            .started_us
            .checked_add(u64::from(ProtocolLimits::ABSOLUTE.max_handshake_duration_ms()) * 1000)
        else {
            return self.fail(BrowserAdapterError::InvalidClock);
        };
        self.deadline_us = until;
        self.phase = Phase::AwaitingHost;
        Ok(out[..length].to_vec())
    }

    /// Every successful transition comes from an exact canonical host record.
    /// A socket opening or a bootstrap field never creates observation.
    pub fn receive_control(
        &mut self,
        attempt: BrowserAttempt,
        bytes: &[u8],
        now_us: u64,
    ) -> Result<&'static str, BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        if !matches!(
            self.phase,
            Phase::AwaitingHost | Phase::AwaitingApproval | Phase::Viewing
        ) {
            return self.fail(BrowserAdapterError::InvalidState);
        }
        if bytes.len() > MAX_CONTROL_RECORD_BYTES {
            return self.fail(BrowserAdapterError::MessageTooLarge);
        }
        let binding = self
            .desktop
            .as_ref()
            .map_or(0, crate::desktop::DesktopSession::control_binding);
        let kind = match browser::record_kind(bytes, MAX_CONTROL_RECORD_BYTES, binding) {
            Ok(kind) => kind,
            Err(_) => return self.fail(BrowserAdapterError::InvalidControl),
        };
        if kind == Kind::Refused {
            let message = match refusal::decode(
                bytes,
                binding,
                MAX_CONTROL_RECORD_BYTES,
                InputDelivery::Reliable,
            ) {
                Ok(message) => message,
                Err(_) => return self.fail(BrowserAdapterError::InvalidControl),
            };
            if message.operation.is_some() || message.stage.is_some() {
                return self.fail(BrowserAdapterError::InvalidControl);
            }
            let Some(code) = host_refusal_code(message.reason) else {
                return self.fail(BrowserAdapterError::InvalidControl);
            };
            self.retire(Phase::Refused, Some(code));
            return Ok(code);
        }
        let result = self
            .desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .receive_control(&mut self.core, bytes, now_us);
        match result {
            Ok(()) => {
                if kind == Kind::ApprovalRequired {
                    // check_current above enforced the original negotiation
                    // deadline before this single positively negotiated notice.
                    let Some(until) = self
                        .started_us
                        .checked_add(browser::APPROVAL_STARTUP_BUDGET_US)
                    else {
                        return self.fail(BrowserAdapterError::InvalidClock);
                    };
                    if self.phase != Phase::AwaitingHost
                        || self
                            .desktop
                            .as_ref()
                            .is_none_or(|desktop| desktop.approval().is_none())
                    {
                        return self.fail(BrowserAdapterError::InvalidControl);
                    }
                    self.deadline_us = until;
                    self.phase = Phase::AwaitingApproval;
                } else if kind == Kind::SessionOpened && self.phase == Phase::AwaitingApproval {
                    self.phase = Phase::AwaitingHost;
                }
                Ok("")
            }
            Err(error) => self.fail(error),
        }
    }

    pub fn take_control(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<Option<Vec<u8>>, BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .take_control()
    }
    pub fn control_queued(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<(), BrowserAdapterError> {
        // The original bytes have just entered the socket. Mark that exact
        // clock probe sent before any tick can abandon or replace it.
        self.require_attempt(attempt)?;
        if now_us < self.last_now_us {
            return self.fail(BrowserAdapterError::InvalidClock);
        }
        self.last_now_us = now_us;
        let result = self
            .desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .control_queued(&mut self.core, now_us);
        match result {
            Ok(()) => Ok(()),
            Err(error) => self.fail(error),
        }
    }
    pub fn needs_video(&self) -> bool {
        self.desktop
            .as_ref()
            .is_some_and(crate::desktop::DesktopSession::needs_video)
    }
    pub fn video_attach(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<Vec<u8>, BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .video_attach()
    }
    pub fn receive_video(
        &mut self,
        attempt: BrowserAttempt,
        bytes: &[u8],
        now_us: u64,
    ) -> Result<&'static str, BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        if bytes.len() > browser::MAX_VIDEO_RECORD_BYTES {
            return self.fail(BrowserAdapterError::MessageTooLarge);
        }
        let result = self
            .desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .receive_video(&mut self.core, bytes, now_us);
        match result {
            Ok(event) => Ok(event),
            Err(error) => self.fail(error),
        }
    }
    pub fn take_video_feedback(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<Option<Vec<u8>>, BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .take_video_feedback()
    }
    pub fn configuration_ready(&self) -> bool {
        self.desktop
            .as_ref()
            .is_some_and(crate::desktop::DesktopSession::configuration_ready)
    }
    pub fn configuration_parameters(&self) -> Result<String, BrowserAdapterError> {
        let parameters = self
            .desktop
            .as_ref()
            .ok_or(BrowserAdapterError::InvalidState)?
            .configuration_parameters()?;
        serde_json::to_string(&parameters).map_err(|_| BrowserAdapterError::Encoding)
    }
    pub fn prepare_desktop_configuration(
        &mut self,
    ) -> Result<crate::media_adapter::PreparedConfiguration, BrowserAdapterError> {
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .prepare_configuration(&mut self.core)
    }
    pub fn desktop_configured(
        &mut self,
        decoder: u64,
        now_us: u64,
    ) -> Result<(), BrowserAdapterError> {
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .configured(&mut self.core, decoder, now_us)
    }
    pub fn take_video_chunk(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<Option<crate::media_adapter::PreparedChunk>, BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        let result = self
            .desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .take_chunk(&mut self.core, now_us);
        match result {
            Ok(chunk) => Ok(chunk),
            Err(error) => self.fail(error),
        }
    }
    /// Native cleanup callbacks keep their original owner after the transport
    /// attempt is retired. They cannot mutate any replacement connection.
    pub fn desktop_output(
        &mut self,
        decoder: u64,
        callback: u64,
        now_us: u64,
    ) -> Result<crate::media_adapter::OutputReceipt, BrowserAdapterError> {
        let result = self
            .desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .output(&mut self.core, decoder, callback, now_us)?;
        if self
            .desktop
            .as_ref()
            .is_some_and(crate::desktop::DesktopSession::ready)
        {
            self.phase = Phase::Viewing;
        }
        Ok(result)
    }
    pub fn desktop_presented(
        &mut self,
        decoder: u64,
        callback: u64,
        now_us: u64,
        visible: bool,
    ) -> Result<(), BrowserAdapterError> {
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .presented(&mut self.core, decoder, callback, now_us, visible)
    }
    pub fn desktop_frame_closed(
        &mut self,
        decoder: u64,
        callback: u64,
        now_us: u64,
    ) -> Result<crate::media_adapter::ReleaseReceipt, BrowserAdapterError> {
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .frame_closed(&mut self.core, decoder, callback, now_us)
    }
    pub fn desktop_decoder_closed(
        &mut self,
        decoder: u64,
    ) -> Result<Vec<crate::media_adapter::ReleaseReceipt>, BrowserAdapterError> {
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .decoder_closed(&mut self.core, decoder)
    }
    pub fn desktop_reclaim(
        &mut self,
        decoder: u64,
        now_us: u64,
    ) -> Result<(), BrowserAdapterError> {
        self.desktop
            .as_mut()
            .ok_or(BrowserAdapterError::InvalidState)?
            .reclaim(&mut self.core, decoder, now_us)
    }
    pub fn desktop_retire(&mut self) {
        if let Some(desktop) = self.desktop.as_mut() {
            desktop.retire_media(&mut self.core);
        }
    }
    pub fn desktop_quarantine(&mut self) {
        if let Some(desktop) = self.desktop.as_mut() {
            desktop.quarantine(&mut self.core);
        }
    }
    pub fn desktop_snapshot(&self) -> crate::media_adapter::MediaSnapshot {
        self.desktop.as_ref().map_or(
            crate::media_adapter::MediaSnapshot {
                active_decoder: None,
                retained_decoders: 0,
                inflight_frames: 0,
                inflight_bytes: 0,
                stage: "awaiting_session",
                quarantined: false,
            },
            |desktop| desktop.snapshot(&self.core),
        )
    }

    pub fn lifecycle(
        &mut self,
        attempt: BrowserAttempt,
        event: &str,
        now_us: u64,
    ) -> Result<(), BrowserAdapterError> {
        self.check_current(attempt, now_us)?;
        let event = match event {
            "hidden" => BrowserLifecycleEvent::TabHidden,
            "visible" => BrowserLifecycleEvent::TabVisible,
            "focus_lost" => BrowserLifecycleEvent::FocusLost,
            "focus_gained" => BrowserLifecycleEvent::FocusGained,
            "pagehide" => BrowserLifecycleEvent::PageHide,
            "bfcache_restore" => BrowserLifecycleEvent::BfcacheRestore,
            _ => return self.fail(BrowserAdapterError::InvalidLifecycle),
        };
        self.core.on_lifecycle_event(event, now_us);
        if matches!(
            event,
            BrowserLifecycleEvent::TabHidden | BrowserLifecycleEvent::FocusLost
        ) {
            if let Some(desktop) = self.desktop.as_mut() {
                desktop.hide();
            }
        }
        if matches!(
            event,
            BrowserLifecycleEvent::PageHide | BrowserLifecycleEvent::BfcacheRestore
        ) {
            self.retire(Phase::Closed, None);
        }
        Ok(())
    }

    pub fn tick(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<(), BrowserAdapterError> {
        self.check_current(attempt, now_us)
    }

    pub fn disconnect(&mut self, attempt: BrowserAttempt) -> Result<(), BrowserAdapterError> {
        self.require_attempt(attempt)?;
        self.retire(Phase::Closed, None);
        Ok(())
    }

    pub fn snapshot(&self) -> BrowserAdapterSnapshot {
        let core = self.core.snapshot();
        BrowserAdapterSnapshot {
            attempt: self.attempt.map(BrowserAttempt::get),
            state_label: if self.phase == Phase::AwaitingHost
                && self
                    .desktop
                    .as_ref()
                    .is_some_and(crate::desktop::DesktopSession::opened)
            {
                "awaiting_media"
            } else {
                self.phase.label()
            },
            transport_label: TRANSPORT.diagnostic_label(),
            visible: core.visible,
            focused: core.focused,
            view_fresh: core.view_fresh,
            can_request_control: false,
            is_controlling: false,
            refusal_code: self.refusal_code,
        }
    }

    pub(crate) fn require_attempt(
        &self,
        attempt: BrowserAttempt,
    ) -> Result<(), BrowserAdapterError> {
        if self.attempt == Some(attempt) {
            Ok(())
        } else {
            Err(BrowserAdapterError::Cancelled)
        }
    }

    #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
    pub(crate) fn invalid_clock(&mut self, attempt: BrowserAttempt) -> BrowserAdapterError {
        if let Err(error) = self.require_attempt(attempt) {
            return error;
        }
        self.retire(
            Phase::Refused,
            Some(BrowserAdapterError::InvalidClock.code()),
        );
        BrowserAdapterError::InvalidClock
    }

    fn check_current(
        &mut self,
        attempt: BrowserAttempt,
        now_us: u64,
    ) -> Result<(), BrowserAdapterError> {
        // Stale callbacks are checked before their time, bytes or state can
        // mutate a replacement connection, including a regressed timestamp.
        self.require_attempt(attempt)?;
        if now_us < self.last_now_us {
            return self.fail(BrowserAdapterError::InvalidClock);
        }
        self.last_now_us = now_us;
        if self.phase != Phase::Viewing && now_us >= self.deadline_us {
            return self.fail(if self.phase == Phase::AwaitingApproval {
                BrowserAdapterError::ApprovalExpired
            } else {
                BrowserAdapterError::Expired
            });
        }
        if self.core.tick(now_us).is_err() {
            return self.fail(BrowserAdapterError::InvalidClock);
        }
        if let Some(desktop) = self.desktop.as_mut() {
            if desktop.opened() {
                if let Err(error) = desktop.tick(&mut self.core, now_us) {
                    return self.fail(error);
                }
            }
        }
        Ok(())
    }

    fn retire(&mut self, phase: Phase, refusal_code: Option<&'static str>) {
        self.attempt = None;
        self.nonce = None;
        self.core.close();
        if let Some(desktop) = self.desktop.as_mut() {
            desktop.close(&mut self.core);
        }
        self.phase = phase;
        self.refusal_code = refusal_code;
    }

    fn fail<T>(&mut self, error: BrowserAdapterError) -> Result<T, BrowserAdapterError> {
        self.retire(Phase::Refused, Some(error.code()));
        Err(error)
    }
}

fn parse_nonce(value: &str) -> Option<[u8; browser::NONCE_BYTES]> {
    if value.len() != browser::NONCE_BYTES * 2 {
        return None;
    }
    let mut nonce = [0; browser::NONCE_BYTES];
    for (out, pair) in nonce.iter_mut().zip(value.as_bytes().as_chunks::<2>().0) {
        let digit = |byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        };
        *out = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(nonce)
}

fn host_refusal_code(reason: Reason) -> Option<&'static str> {
    Some(match reason {
        Reason::InvalidMessage => "host_invalid_message",
        Reason::UnsupportedVersion => "host_unsupported_version",
        Reason::UnsupportedProfile => "host_unsupported_profile",
        Reason::RequiredCapability => "host_required_capability_missing",
        Reason::InvalidLimits => "host_invalid_limits",
        Reason::InvalidSelection => "host_invalid_selection",
        Reason::PermissionDenied => "host_permission_denied",
        Reason::LocalApprovalDenied => "host_local_approval_denied",
        Reason::ApprovalExpired => "host_approval_expired",
        Reason::ControlUnavailable => "host_control_unavailable",
        Reason::ResourceLimit => "host_resource_limit",
        Reason::InvalidState => "host_invalid_state",
        Reason::Expired => "host_expired",
        Reason::HostUnavailable => "host_unavailable",
        Reason::TailnetMembershipUnverifiable => "host_tailnet_membership_unverifiable",
        // These can only answer a directory request; none was sent here.
        Reason::DirectoryDisabled | Reason::DirectoryUnavailable => return None,
    })
}
