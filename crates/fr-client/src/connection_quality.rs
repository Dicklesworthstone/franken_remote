//! Connection quality metrics and health evaluation for client desktop chrome (Plan §16.1).
#![forbid(unsafe_code)]

/// Underlying transport type negotiated for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportType {
    /// High-performance native QUIC datagram + stream transport.
    NativeQuic,
    /// Bounded WebSocket transport profile (fallback).
    NativeWss,
}

/// Real-time connection quality telemetry collected by the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionQualityMetrics {
    /// Measured round-trip time in milliseconds.
    pub rtt_ms: u32,
    /// One-way jitter estimate in milliseconds.
    pub jitter_ms: u32,
    /// Packet loss in permille (0 to 1000, where 10 = 1.0%).
    pub packet_loss_permille: u16,
    /// Video presentation framerate in frames per second.
    pub fps: u16,
    /// Video downlink bitrate in kilobits per second.
    pub bitrate_kbps: u32,
    /// Foreign hardware decode time in microseconds.
    pub decode_time_us: u32,
    /// Surface swapchain present time in microseconds.
    pub render_time_us: u32,
    /// Active transport protocol.
    pub transport: TransportType,
}

impl Default for ConnectionQualityMetrics {
    fn default() -> Self {
        Self {
            rtt_ms: 0,
            jitter_ms: 0,
            packet_loss_permille: 0,
            fps: 60,
            bitrate_kbps: 0,
            decode_time_us: 0,
            render_time_us: 0,
            transport: TransportType::NativeQuic,
        }
    }
}

/// High-level categorized health tier for UI presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum QualityTier {
    /// Latency < 30ms, 0% loss, full 60+ fps.
    Excellent,
    /// Latency < 60ms, < 0.5% loss, acceptable framerate.
    Good,
    /// Latency 60-120ms or minor packet loss; responsive but perceptible.
    Degraded,
    /// Latency > 120ms or significant packet loss (> 2%); input paused or degraded.
    Poor,
}

/// Actionable diagnostic warnings emitted during connection degradation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QualityWarning {
    HighLatency { rtt_ms: u32 },
    HighJitter { jitter_ms: u32 },
    PacketLoss { permille: u16 },
    DecodeOverrun { decode_us: u32 },
    FramerateStarvation { fps: u16 },
}

impl ConnectionQualityMetrics {
    /// Classify current connection metrics into a health tier and typed warnings.
    pub fn evaluate(&self) -> (QualityTier, Vec<QualityWarning>) {
        let mut warnings = Vec::new();

        if self.rtt_ms >= 120 {
            warnings.push(QualityWarning::HighLatency {
                rtt_ms: self.rtt_ms,
            });
        }
        if self.jitter_ms >= 30 {
            warnings.push(QualityWarning::HighJitter {
                jitter_ms: self.jitter_ms,
            });
        }
        if self.packet_loss_permille >= 20 {
            warnings.push(QualityWarning::PacketLoss {
                permille: self.packet_loss_permille,
            });
        }
        if self.decode_time_us >= 16_000 {
            warnings.push(QualityWarning::DecodeOverrun {
                decode_us: self.decode_time_us,
            });
        }
        if self.fps < 30 && self.bitrate_kbps > 500 {
            warnings.push(QualityWarning::FramerateStarvation { fps: self.fps });
        }

        let tier = if self.rtt_ms > 120 || self.packet_loss_permille > 20 || self.fps < 20 {
            QualityTier::Poor
        } else if self.rtt_ms > 60 || self.packet_loss_permille > 5 || self.jitter_ms > 20 {
            QualityTier::Degraded
        } else if self.rtt_ms > 30 || self.packet_loss_permille > 0 {
            QualityTier::Good
        } else {
            QualityTier::Excellent
        };

        (tier, warnings)
    }

    /// User-facing short status badge text for minimal toolbar display.
    pub fn badge_text(&self) -> &'static str {
        match self.evaluate().0 {
            QualityTier::Excellent => "Excellent",
            QualityTier::Good => "Good",
            QualityTier::Degraded => "Degraded",
            QualityTier::Poor => "Poor",
        }
    }
}
