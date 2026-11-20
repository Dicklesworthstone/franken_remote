//! Secure WebSocket (WSS) compatibility profile (Plan §§12.5, 17.3).
//!
//! Explicit degraded fallback for environments without qualified WebTransport
//! or UDP datagram capability (e.g. restrictive corporate proxies, certain browsers).
#![forbid(unsafe_code)]

use std::collections::HashSet;

/// Diagnostic labeling for the WSS fallback profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WssProfile {
    /// Always true for WSS: TCP enforces in-order byte delivery.
    pub tcp_head_of_line: bool,
    /// Whether multiple logical channels share a single lower TCP connection.
    pub multiplexed_transport: bool,
    /// Per-message compression is disabled for media and sensitive control.
    pub compression_disabled: bool,
}

impl WssProfile {
    #[must_use]
    pub const fn new(multiplexed: bool) -> Self {
        Self {
            tcp_head_of_line: true,
            multiplexed_transport: multiplexed,
            compression_disabled: true,
        }
    }

    #[must_use]
    pub const fn diagnostic_label(&self) -> &'static str {
        if self.multiplexed_transport {
            "wss_degraded_tcp_hol_multiplexed"
        } else {
            "wss_degraded_tcp_hol_dedicated"
        }
    }
}

/// Distinct functional roles for separately bound WSS channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WssChannelRole {
    Control,
    Video,
    Audio,
}

/// Role-specific one-use capability token for channel attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelToken {
    pub role: WssChannelRole,
    pub token_id: u64,
    pub consumed: bool,
}

impl ChannelToken {
    #[must_use]
    pub const fn new(role: WssChannelRole, token_id: u64) -> Self {
        Self {
            role,
            token_id,
            consumed: false,
        }
    }

    pub fn consume(&mut self, requested_role: WssChannelRole) -> Result<(), WssError> {
        if self.consumed {
            return Err(WssError::TokenAlreadyConsumed);
        }
        if self.role != requested_role {
            return Err(WssError::RoleMismatch(self.role, requested_role));
        }
        self.consumed = true;
        Ok(())
    }
}

/// Distinguishable stages for receiver acknowledgements. None proves that
/// receive allocations or codec references were freed, so these observations
/// cannot return resource credit. Only [`ReleasedCredit`] can do that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WssAck {
    Received(u64, usize),
    Decoded(u64, usize),
    Presented(u64, usize),
}

impl WssAck {
    #[must_use]
    pub const fn bytes(&self) -> usize {
        match *self {
            Self::Received(_, b) | Self::Decoded(_, b) | Self::Presented(_, b) => b,
        }
    }

    #[must_use]
    pub const fn frame_id(&self) -> u64 {
        match *self {
            Self::Received(f, _) | Self::Decoded(f, _) | Self::Presented(f, _) => f,
        }
    }
}

/// Cumulative receive resources actually freed in one negotiated credit epoch.
/// These are totals since attachment, not additional grants or frame IDs.
/// They may exceed the initial allowance during a long-running session.
///
/// The receiver must include retained codec references and charged metadata in
/// its accounting. Receipt, decode, presentation, socket acceptance and transport
/// acknowledgements alone do not establish that those resources were released.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedCredit {
    pub credit_epoch: u64,
    pub released_bytes: u64,
    pub released_frames: u64,
}

impl ReleasedCredit {
    /// Extract cumulative freed resources from a validated `0x0081 ReceiverPressure` record.
    #[must_use]
    pub const fn from_wire_pressure(
        report: &fr_wire::receiver_pressure::ReceiverPressure,
    ) -> Self {
        Self {
            credit_epoch: report.credit_epoch,
            released_bytes: report.released_bytes,
            released_frames: report.released_frames,
        }
    }
}

/// Whether a validated release report advanced either cumulative counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreditUpdate {
    Applied,
    Ignored,
}

/// Single sender owner of an attached channel's byte and frame allowance.
///
/// Per PROTOCOL.md §5, outstanding use is cumulative consumption minus cumulative
/// released resources. Both dimensions must fit the initial allowance. Reserve
/// the complete charged allocation, including metadata, before sending a unit;
/// reserving only its first fragment would not bound receiver memory.
///
/// This owner cannot be cloned or reset. Channel replacement needs a freshly
/// negotiated epoch and a new window, never reused counters under an old epoch.
/// This is accounting policy, not a WSS endpoint or proof of receiver release.
#[derive(Debug)]
pub struct ReceiverCreditWindow {
    credit_epoch: u64,
    max_bytes: u64,
    max_frames: u64,
    consumed_bytes: u64,
    consumed_frames: u64,
    released_bytes: u64,
    released_frames: u64,
}

impl ReceiverCreditWindow {
    /// Install the negotiated epoch and positive local allowances from the
    /// admitted attachment. `PROTOCOL_ATTACHMENT.md` rejects credit epoch zero;
    /// unlike a typed initial generation, it cannot identify a credit owner.
    pub const fn new(credit_epoch: u64, max_bytes: u64, max_frames: u64) -> Result<Self, WssError> {
        if credit_epoch == 0 || max_bytes == 0 || max_frames == 0 {
            return Err(WssError::InvalidCreditWindow);
        }
        Ok(Self {
            credit_epoch,
            max_bytes,
            max_frames,
            consumed_bytes: 0,
            consumed_frames: 0,
            released_bytes: 0,
            released_frames: 0,
        })
    }

    #[must_use]
    pub const fn credit_epoch(&self) -> u64 {
        self.credit_epoch
    }

    #[must_use]
    pub fn can_send(&self, bytes: u64) -> bool {
        self.reservation(bytes).is_ok()
    }

    /// Consume one complete frame's nonzero allocation cost. Failure leaves
    /// both counters untouched; releasing resources never resets consumption.
    pub fn try_reserve(&mut self, bytes: u64) -> Result<(), WssError> {
        let (consumed_bytes, consumed_frames) = self.reservation(bytes)?;
        self.consumed_bytes = consumed_bytes;
        self.consumed_frames = consumed_frames;
        Ok(())
    }

    fn reservation(&self, bytes: u64) -> Result<(u64, u64), WssError> {
        if bytes == 0 {
            return Err(WssError::InvalidCreditCharge);
        }
        let consumed_bytes = self
            .consumed_bytes
            .checked_add(bytes)
            .ok_or(WssError::CreditCounterExhausted)?;
        let consumed_frames = self
            .consumed_frames
            .checked_add(1)
            .ok_or(WssError::CreditCounterExhausted)?;
        if consumed_bytes - self.released_bytes > self.max_bytes
            || consumed_frames - self.released_frames > self.max_frames
        {
            return Err(WssError::CreditStarvation(
                self.outstanding_bytes(),
                self.max_bytes,
                self.outstanding_frames(),
                self.max_frames,
            ));
        }
        Ok((consumed_bytes, consumed_frames))
    }

    /// Apply cumulative freed-resource totals, independently ignoring duplicate
    /// or older counters. Validate the epoch and BOTH upper bounds first: one
    /// valid field cannot grant credit when the other field is malformed.
    /// Neither acknowledgement stages nor new allowances are accepted here.
    pub fn return_credit(&mut self, release: ReleasedCredit) -> Result<CreditUpdate, WssError> {
        if release.credit_epoch != self.credit_epoch {
            return Err(WssError::CreditEpochMismatch {
                expected: self.credit_epoch,
                received: release.credit_epoch,
            });
        }
        if release.released_bytes > self.consumed_bytes
            || release.released_frames > self.consumed_frames
        {
            return Err(WssError::CreditReleaseExceedsConsumption);
        }
        let bytes = self.released_bytes.max(release.released_bytes);
        let frames = self.released_frames.max(release.released_frames);
        if bytes == self.released_bytes && frames == self.released_frames {
            return Ok(CreditUpdate::Ignored);
        }
        self.released_bytes = bytes;
        self.released_frames = frames;
        Ok(CreditUpdate::Applied)
    }

    /// Apply a decoded `0x0081 ReceiverPressure` wire record to this credit
    /// window after verifying that its compact binding ID and channel role match
    /// the attached WSS channel. Never grants authority or resets consumption.
    pub fn apply_wire_pressure(
        &mut self,
        expected_binding: u32,
        expected_role: fr_wire::receiver_pressure::ChannelRole,
        report: &fr_wire::receiver_pressure::ReceiverPressure,
    ) -> Result<CreditUpdate, WssError> {
        if expected_binding == 0 || report.channel_binding != expected_binding {
            return Err(WssError::StaleGeneration(
                ChannelGeneration(u64::from(expected_binding)),
                ChannelGeneration(u64::from(report.channel_binding)),
            ));
        }
        if report.channel_role != expected_role {
            return Err(WssError::RoleMismatch(
                wss_role_from_wire(expected_role),
                wss_role_from_wire(report.channel_role),
            ));
        }
        self.return_credit(ReleasedCredit::from_wire_pressure(report))
    }

    #[must_use]
    pub const fn outstanding_bytes(&self) -> u64 {
        self.consumed_bytes - self.released_bytes
    }

    #[must_use]
    pub const fn outstanding_frames(&self) -> u64 {
        self.consumed_frames - self.released_frames
    }
}

const fn wss_role_from_wire(role: fr_wire::receiver_pressure::ChannelRole) -> WssChannelRole {
    match role {
        fr_wire::receiver_pressure::ChannelRole::Video => WssChannelRole::Video,
        fr_wire::receiver_pressure::ChannelRole::AudioDown
        | fr_wire::receiver_pressure::ChannelRole::AudioUp => WssChannelRole::Audio,
        fr_wire::receiver_pressure::ChannelRole::Clipboard
        | fr_wire::receiver_pressure::ChannelRole::Files
        | fr_wire::receiver_pressure::ChannelRole::FilesDownload => WssChannelRole::Control,
    }
}

/// Monotonic generation counter for fencing obsolete channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChannelGeneration(pub u64);

/// Channel supervisor enforcing generation fencing and bounded closing sets.
#[derive(Debug)]
pub struct ChannelSupervisor {
    role: WssChannelRole,
    active_generation: ChannelGeneration,
    closing_generations: HashSet<ChannelGeneration>,
    max_closing_channels: usize,
}

impl ChannelSupervisor {
    #[must_use]
    pub fn new(role: WssChannelRole, max_closing_channels: usize) -> Self {
        Self {
            role,
            active_generation: ChannelGeneration(1),
            closing_generations: HashSet::new(),
            max_closing_channels,
        }
    }

    #[must_use]
    pub const fn active_generation(&self) -> ChannelGeneration {
        self.active_generation
    }

    #[must_use]
    pub const fn role(&self) -> WssChannelRole {
        self.role
    }

    /// Replaces the active channel with a fresh generation, immediately fencing the old one.
    pub fn replace_channel(&mut self) -> Result<ChannelGeneration, WssError> {
        let next = self
            .active_generation
            .0
            .checked_add(1)
            .map(ChannelGeneration)
            .ok_or(WssError::GenerationExhausted)?;
        if self.closing_generations.len() >= self.max_closing_channels {
            return Err(WssError::ResetFloodThrottled(
                self.closing_generations.len(),
            ));
        }
        self.closing_generations.insert(self.active_generation);
        self.active_generation = next;
        Ok(self.active_generation)
    }

    /// Completes the close of a fenced generation.
    pub fn complete_close(&mut self, generation: ChannelGeneration) {
        self.closing_generations.remove(&generation);
    }

    /// Checks if a message with the given generation is accepted.
    pub fn check_admission(&self, generation: ChannelGeneration) -> Result<(), WssError> {
        if generation != self.active_generation {
            return Err(WssError::StaleGeneration(
                self.active_generation,
                generation,
            ));
        }
        Ok(())
    }
}

/// Payload type discriminator for multiplexed channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    Control,
    Video,
    Audio,
    Input,
    Clipboard,
}

/// Verifies input lease retention based on view freshness (not merely socket-open state).
pub const fn check_input_lease_retention(
    socket_open: bool,
    view_fresh: bool,
) -> Result<(), WssError> {
    if !socket_open {
        return Err(WssError::SocketClosed);
    }
    if !view_fresh {
        return Err(WssError::InputSuspendedStaleView);
    }
    Ok(())
}

/// Validates that clipboard is never multiplexed on the input channel.
pub const fn validate_channel_payload(
    channel: WssChannelRole,
    payload: PayloadKind,
) -> Result<(), WssError> {
    if matches!(channel, WssChannelRole::Control) && matches!(payload, PayloadKind::Clipboard) {
        return Err(WssError::ClipboardNotAllowedOnInput);
    }
    Ok(())
}

/// Rejects downgrade attempts that would bypass required security checks.
pub const fn validate_security_downgrade(
    tls_valid: bool,
    tailscale_authenticated: bool,
) -> Result<(), WssError> {
    if !tls_valid || !tailscale_authenticated {
        return Err(WssError::SecurityDowngradeRefused);
    }
    Ok(())
}

/// Compression is always disabled for WSS media and sensitive control.
#[must_use]
pub const fn is_compression_allowed(_role: WssChannelRole) -> bool {
    false
}

/// Errors specific to WSS degraded fallback transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WssError {
    RoleMismatch(WssChannelRole, WssChannelRole),
    TokenAlreadyConsumed,
    InvalidCreditWindow,
    InvalidCreditCharge,
    CreditStarvation(u64, u64, u64, u64),
    CreditEpochMismatch { expected: u64, received: u64 },
    CreditReleaseExceedsConsumption,
    CreditCounterExhausted,
    StaleGeneration(ChannelGeneration, ChannelGeneration),
    GenerationExhausted,
    ResetFloodThrottled(usize),
    SocketClosed,
    InputSuspendedStaleView,
    ClipboardNotAllowedOnInput,
    SecurityDowngradeRefused,
}

#[cfg(test)]
#[path = "wss/credit_tests.rs"]
mod credit_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn honest_degradation_labeling_and_compression_policy() {
        let dedicated = WssProfile::new(false);
        assert!(dedicated.tcp_head_of_line);
        assert!(!dedicated.multiplexed_transport);
        assert!(dedicated.compression_disabled);
        assert_eq!(
            dedicated.diagnostic_label(),
            "wss_degraded_tcp_hol_dedicated"
        );

        let multiplexed = WssProfile::new(true);
        assert!(multiplexed.multiplexed_transport);
        assert_eq!(
            multiplexed.diagnostic_label(),
            "wss_degraded_tcp_hol_multiplexed"
        );

        for role in [
            WssChannelRole::Control,
            WssChannelRole::Video,
            WssChannelRole::Audio,
        ] {
            assert!(!is_compression_allowed(role));
        }
    }

    #[test]
    fn role_specific_capability_tokens_prevent_media_taking_control() {
        let mut video_token = ChannelToken::new(WssChannelRole::Video, 42);
        assert_eq!(
            video_token.consume(WssChannelRole::Control),
            Err(WssError::RoleMismatch(
                WssChannelRole::Video,
                WssChannelRole::Control
            ))
        );
        assert!(!video_token.consumed);
        assert_eq!(video_token.consume(WssChannelRole::Video), Ok(()));
        assert!(video_token.consumed);
        assert_eq!(
            video_token.consume(WssChannelRole::Video),
            Err(WssError::TokenAlreadyConsumed)
        );
    }

    #[test]
    fn stale_generation_fencing_and_closing_channel_bounds() {
        let mut supervisor = ChannelSupervisor::new(WssChannelRole::Video, 2);
        let gen1 = supervisor.active_generation();
        assert_eq!(supervisor.check_admission(gen1), Ok(()));

        let gen2 = supervisor.replace_channel().unwrap();
        assert_eq!(gen2, ChannelGeneration(2));
        assert_eq!(supervisor.check_admission(gen2), Ok(()));
        // Old generation is immediately rejected
        assert_eq!(
            supervisor.check_admission(gen1),
            Err(WssError::StaleGeneration(gen2, gen1))
        );

        let gen3 = supervisor.replace_channel().unwrap();
        assert_eq!(gen3, ChannelGeneration(3));

        // Limit of 2 closing channels reached
        assert_eq!(
            supervisor.replace_channel(),
            Err(WssError::ResetFloodThrottled(2))
        );

        // Complete close of gen1 frees a slot
        supervisor.complete_close(gen1);
        let gen4 = supervisor.replace_channel().unwrap();
        assert_eq!(gen4, ChannelGeneration(4));
    }

    #[test]
    fn generation_exhaustion_neither_reuses_ids_nor_mutates_closing_owners() {
        let mut supervisor = ChannelSupervisor::new(WssChannelRole::Video, 3);
        supervisor.active_generation = ChannelGeneration(u64::MAX - 1);
        assert_eq!(
            supervisor.replace_channel(),
            Ok(ChannelGeneration(u64::MAX))
        );
        let closing = supervisor.closing_generations.clone();
        assert_eq!(
            supervisor.replace_channel(),
            Err(WssError::GenerationExhausted)
        );
        assert_eq!(supervisor.active_generation(), ChannelGeneration(u64::MAX));
        assert_eq!(supervisor.closing_generations, closing);
        assert_eq!(
            supervisor.check_admission(ChannelGeneration(0)),
            Err(WssError::StaleGeneration(
                ChannelGeneration(u64::MAX),
                ChannelGeneration(0),
            ))
        );
        supervisor.complete_close(ChannelGeneration(u64::MAX - 1));
        assert_eq!(
            supervisor.replace_channel(),
            Err(WssError::GenerationExhausted)
        );
        assert!(supervisor.closing_generations.is_empty());
    }

    #[test]
    fn input_lease_view_freshness_and_payload_isolation() {
        // Socket open is not enough: view freshness is strictly required
        assert_eq!(check_input_lease_retention(true, true), Ok(()));
        assert_eq!(
            check_input_lease_retention(true, false),
            Err(WssError::InputSuspendedStaleView)
        );
        assert_eq!(
            check_input_lease_retention(false, true),
            Err(WssError::SocketClosed)
        );

        // Clipboard forbidden on input/control channel
        assert_eq!(
            validate_channel_payload(WssChannelRole::Control, PayloadKind::Clipboard),
            Err(WssError::ClipboardNotAllowedOnInput)
        );
        assert_eq!(
            validate_channel_payload(WssChannelRole::Control, PayloadKind::Input),
            Ok(())
        );

        // Security downgrade attempts refused
        assert_eq!(validate_security_downgrade(true, true), Ok(()));
        assert_eq!(
            validate_security_downgrade(false, true),
            Err(WssError::SecurityDowngradeRefused)
        );
        assert_eq!(
            validate_security_downgrade(true, false),
            Err(WssError::SecurityDowngradeRefused)
        );
    }

    #[test]
    fn receiver_pressure_wire_record_unlocks_and_fences_wss_credit() {
        use fr_core::{
            ids::{HostBootId, OsSessionId, RemoteSessionId},
            limits::ProtocolLimits,
        };
        use fr_wire::{
            input::{InputDelivery, InputDirection},
            negotiation::ControlBinding,
            receiver_pressure::{self, ChannelRole, QueuePressure, ReceiverPressure},
        };

        let limits = ProtocolLimits::ABSOLUTE;
        let control = ControlBinding {
            id: 7,
            host_boot: HostBootId::from_raw(11),
            os_session: OsSessionId::from_raw(22),
            remote_session: RemoteSessionId::from_raw(33),
        };
        let mut window = ReceiverCreditWindow::new(9, 4096, 2).unwrap();
        window.try_reserve(2048).unwrap();
        window.try_reserve(2048).unwrap();
        assert!(matches!(
            window.try_reserve(1024),
            Err(WssError::CreditStarvation(4096, 4096, 2, 2))
        ));

        let report = ReceiverPressure {
            channel_binding: 55,
            channel_role: ChannelRole::Video,
            credit_epoch: 9,
            released_bytes: 2048,
            released_frames: 1,
            pressure: QueuePressure::Moderate,
            queued_bytes: 2048,
            queued_items: 1,
        };
        let mut buf = [0_u8; receiver_pressure::RECORD_BYTES];
        let written = receiver_pressure::encode(
            &report,
            control,
            &limits,
            &mut buf,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        let decoded = receiver_pressure::decode(
            &buf[..written],
            control,
            &limits,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();

        // Wrong channel binding or role is fenced before crediting.
        assert_eq!(
            window.apply_wire_pressure(56, ChannelRole::Video, &decoded),
            Err(WssError::StaleGeneration(
                ChannelGeneration(56),
                ChannelGeneration(55),
            ))
        );
        assert_eq!(
            window.apply_wire_pressure(55, ChannelRole::AudioDown, &decoded),
            Err(WssError::RoleMismatch(
                WssChannelRole::Audio,
                WssChannelRole::Video,
            ))
        );
        assert_eq!(window.outstanding_bytes(), 4096);

        // Matching wire record unlocks credit and allows the next frame.
        assert_eq!(
            window.apply_wire_pressure(55, ChannelRole::Video, &decoded),
            Ok(CreditUpdate::Applied)
        );
        assert_eq!(window.outstanding_bytes(), 2048);
        assert_eq!(window.outstanding_frames(), 1);
        window.try_reserve(2048).unwrap();
        assert_eq!(window.outstanding_bytes(), 4096);
    }
}
