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

/// Distinguishable stages for receiver acknowledgements.
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

/// Receiver-granted outstanding-byte and frame credit window.
#[derive(Debug, Clone)]
pub struct ReceiverCreditWindow {
    max_bytes: usize,
    max_frames: usize,
    outstanding_bytes: usize,
    outstanding_frames: usize,
}

impl ReceiverCreditWindow {
    #[must_use]
    pub const fn new(max_bytes: usize, max_frames: usize) -> Self {
        Self {
            max_bytes,
            max_frames,
            outstanding_bytes: 0,
            outstanding_frames: 0,
        }
    }

    #[must_use]
    pub const fn can_send(&self, bytes: usize) -> bool {
        self.outstanding_frames < self.max_frames
            && self.outstanding_bytes.saturating_add(bytes) <= self.max_bytes
    }

    pub fn try_reserve(&mut self, bytes: usize) -> Result<(), WssError> {
        if !self.can_send(bytes) {
            return Err(WssError::CreditStarvation(
                self.outstanding_bytes,
                self.max_bytes,
                self.outstanding_frames,
                self.max_frames,
            ));
        }
        self.outstanding_bytes += bytes;
        self.outstanding_frames += 1;
        Ok(())
    }

    pub fn return_credit(&mut self, ack: WssAck) {
        let freed_bytes = ack.bytes().min(self.outstanding_bytes);
        self.outstanding_bytes -= freed_bytes;
        self.outstanding_frames = self.outstanding_frames.saturating_sub(1);
    }

    #[must_use]
    pub const fn outstanding_bytes(&self) -> usize {
        self.outstanding_bytes
    }

    #[must_use]
    pub const fn outstanding_frames(&self) -> usize {
        self.outstanding_frames
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
        if self.closing_generations.len() >= self.max_closing_channels {
            return Err(WssError::ResetFloodThrottled(
                self.closing_generations.len(),
            ));
        }
        self.closing_generations.insert(self.active_generation);
        self.active_generation = ChannelGeneration(self.active_generation.0 + 1);
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
    CreditStarvation(usize, usize, usize, usize),
    StaleGeneration(ChannelGeneration, ChannelGeneration),
    ResetFloodThrottled(usize),
    SocketClosed,
    InputSuspendedStaleView,
    ClipboardNotAllowedOnInput,
    SecurityDowngradeRefused,
}

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
    fn receiver_credit_window_enforces_byte_and_frame_bounds_and_ack_release() {
        let mut credit = ReceiverCreditWindow::new(10_000, 3);
        assert!(credit.can_send(5_000));
        assert_eq!(credit.try_reserve(4_000), Ok(()));
        assert_eq!(credit.outstanding_bytes(), 4_000);
        assert_eq!(credit.outstanding_frames(), 1);

        assert_eq!(credit.try_reserve(4_000), Ok(()));
        assert_eq!(credit.outstanding_bytes(), 8_000);
        assert_eq!(credit.outstanding_frames(), 2);

        // Third frame fits bytes but hits frame count
        assert_eq!(credit.try_reserve(1_000), Ok(()));
        assert_eq!(credit.outstanding_frames(), 3);

        // Frame count exhausted
        assert!(!credit.can_send(500));
        assert!(matches!(
            credit.try_reserve(500),
            Err(WssError::CreditStarvation(..))
        ));

        // Distinguishable ACKs free credit
        credit.return_credit(WssAck::Received(1, 4_000));
        assert_eq!(credit.outstanding_bytes(), 5_000);
        assert_eq!(credit.outstanding_frames(), 2);
        assert!(credit.can_send(2_000));

        credit.return_credit(WssAck::Decoded(2, 4_000));
        credit.return_credit(WssAck::Presented(3, 1_000));
        assert_eq!(credit.outstanding_bytes(), 0);
        assert_eq!(credit.outstanding_frames(), 0);
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
}
