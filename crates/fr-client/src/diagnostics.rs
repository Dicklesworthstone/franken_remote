//! Bounded, sanitized session diagnostics, "why does this feel slow?" causal
//! attribution, and deterministic controller-replay bundles (Plan §§3.4, 18.4).
//!
//! # Privacy & Bounds Invariants
//! - Bounded in BOTH event count and retained byte weight; oldest records evict
//!   first and oversized single events are refused before storage.
//! - Content-free by construction: pixels, clipboard text, typed input, file
//!   contents, credentials, TLS keys, and session nonces have no representation
//!   in any diagnostic type.
//! - Any free-form category note passes through [`sanitize_diagnostic_text`]
//!   before entering the ring and again on bundle export, scrubbing hostnames,
//!   IP addresses, user/filesystem paths, email addresses, PEM/key markers,
//!   tokens, and quoted window titles.
//! - Export is rate-limited using `ProtocolLimits::max_diagnostic_exports_per_min`.
//! - Never invents a slowness cause without retained evidence: absence of
//!   samples reports [`SlownessVerdict::InsufficientEvidence`].
#![forbid(unsafe_code)]

use fr_core::limits::ProtocolLimits;
use fr_media::quality::{
    DecisionReason, DecoderMetrics, EncoderMetrics, InputSnapshot, NetworkMetrics, OperatingPoint,
    PresentationMetrics, QualityController, QualityDecision, QualityError, QualityPolicy,
    WorkstationProfile,
};
use fr_wire::receiver_metrics::Load as WireDecoderLoad;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const BUNDLE_SCHEMA_VERSION: u16 = 1;
pub const DEFAULT_MAX_EVENTS: usize = 512;
pub const MAX_ALLOWED_EVENTS: usize = 4_096;
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;
pub const MAX_ALLOWED_BYTES: usize = 1024 * 1024;
pub const MAX_NOTE_BYTES: usize = 160;
pub const MAX_IMPORT_JSON_BYTES: usize = 2 * 1024 * 1024;
const EXPORT_WINDOW_US: u64 = 60_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticError {
    InvalidLimits,
    EventTooLarge,
    ClockRegression,
    ExportRateLimited,
    BundleTooLarge,
    UnsupportedSchemaVersion(u16),
    InvalidPolicy,
    SerializationFailed,
    DeserializationFailed,
    ReplayError(QualityError),
}

impl std::fmt::Display for DiagnosticError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "diagnostics: {self:?}")
    }
}

impl std::error::Error for DiagnosticError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticLimits {
    pub max_events: usize,
    pub max_bytes: usize,
}

impl Default for DiagnosticLimits {
    fn default() -> Self {
        Self {
            max_events: DEFAULT_MAX_EVENTS,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl DiagnosticLimits {
    pub fn validate(self) -> Result<(), DiagnosticError> {
        if self.max_events == 0
            || self.max_events > MAX_ALLOWED_EVENTS
            || self.max_bytes < 512
            || self.max_bytes > MAX_ALLOWED_BYTES
        {
            return Err(DiagnosticError::InvalidLimits);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    NativeQuic,
    WebTransportH3,
    WssDegraded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkPathKind {
    Direct,
    RelayedDerp,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodecPathKind {
    HardwareVideoToolbox,
    HardwareNvenc,
    HardwareVaapi,
    HardwareMediaFoundation,
    HardwareMediaCodec,
    SoftwareExplicit,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceCopyMode {
    ZeroCopyGpu,
    GpuToGpuShared,
    CpuReadback,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueStage {
    Capture,
    Encode,
    TransportSend,
    Reassembly,
    Decode,
    Presentation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryEventKind {
    SelectiveRepair,
    ReliableIdrRecovery,
    DecoderReset,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteCategory {
    Transport,
    Media,
    Authority,
    Lifecycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileRepr {
    Auto,
    Text,
    Motion,
}

impl From<WorkstationProfile> for ProfileRepr {
    fn from(value: WorkstationProfile) -> Self {
        match value {
            WorkstationProfile::Auto => Self::Auto,
            WorkstationProfile::Text => Self::Text,
            WorkstationProfile::Motion => Self::Motion,
        }
    }
}

impl From<ProfileRepr> for WorkstationProfile {
    fn from(value: ProfileRepr) -> Self {
        match value {
            ProfileRepr::Auto => Self::Auto,
            ProfileRepr::Text => Self::Text,
            ProfileRepr::Motion => Self::Motion,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyRepr {
    pub min_bitrate_bps: u64,
    pub max_bitrate_bps: u64,
    pub initial_bitrate_bps: u64,
    pub aggregate_ceiling_bps: u64,
    pub default_profile: ProfileRepr,
    pub min_frame_interval_us: u64,
    pub max_frame_interval_us: u64,
}

impl From<QualityPolicy> for PolicyRepr {
    fn from(p: QualityPolicy) -> Self {
        Self {
            min_bitrate_bps: p.min_bitrate_bps,
            max_bitrate_bps: p.max_bitrate_bps,
            initial_bitrate_bps: p.initial_bitrate_bps,
            aggregate_ceiling_bps: p.aggregate_ceiling_bps,
            default_profile: p.default_profile.into(),
            min_frame_interval_us: p.min_frame_interval_us,
            max_frame_interval_us: p.max_frame_interval_us,
        }
    }
}

impl From<PolicyRepr> for QualityPolicy {
    fn from(p: PolicyRepr) -> Self {
        Self {
            min_bitrate_bps: p.min_bitrate_bps,
            max_bitrate_bps: p.max_bitrate_bps,
            initial_bitrate_bps: p.initial_bitrate_bps,
            aggregate_ceiling_bps: p.aggregate_ceiling_bps,
            default_profile: p.default_profile.into(),
            min_frame_interval_us: p.min_frame_interval_us,
            max_frame_interval_us: p.max_frame_interval_us,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecoderLoadRepr {
    pub retained_bytes: u64,
    pub retained_pictures: u32,
    pub decoding: bool,
    pub work_us: Option<u64>,
}

impl From<WireDecoderLoad> for DecoderLoadRepr {
    fn from(l: WireDecoderLoad) -> Self {
        Self {
            retained_bytes: l.retained_bytes,
            retained_pictures: l.retained_pictures,
            decoding: l.decoding,
            work_us: l.work_us,
        }
    }
}

impl From<DecoderLoadRepr> for WireDecoderLoad {
    fn from(l: DecoderLoadRepr) -> Self {
        Self {
            retained_bytes: l.retained_bytes,
            retained_pictures: l.retained_pictures,
            decoding: l.decoding,
            work_us: l.work_us,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct SnapshotRepr {
    pub now_us: u64,
    pub rtt_us: Option<u64>,
    pub loss_basis_points: Option<u16>,
    pub send_queue_bytes: u64,
    pub send_queue_capacity: u64,
    pub sustainable_delivery_estimate_bps: Option<u64>,
    pub encode_latency_us: Option<u64>,
    pub in_flight_surfaces: u32,
    pub decoder_load: Option<DecoderLoadRepr>,
    pub is_visible: bool,
    pub presentation_latency_us: Option<u64>,
    pub application_limited: bool,
    pub route_changed: bool,
    pub reconnected: bool,
}

impl From<InputSnapshot> for SnapshotRepr {
    fn from(s: InputSnapshot) -> Self {
        Self {
            now_us: s.now_us,
            rtt_us: s.network.rtt_us,
            loss_basis_points: s.network.loss_basis_points,
            send_queue_bytes: s.network.send_queue_bytes,
            send_queue_capacity: s.network.send_queue_capacity,
            sustainable_delivery_estimate_bps: s.network.sustainable_delivery_estimate_bps,
            encode_latency_us: s.encoder.encode_latency_us,
            in_flight_surfaces: s.encoder.in_flight_surfaces,
            decoder_load: s.decoder.load.map(Into::into),
            is_visible: s.presentation.is_visible,
            presentation_latency_us: s.presentation.presentation_latency_us,
            application_limited: s.application_limited,
            route_changed: s.route_changed,
            reconnected: s.reconnected,
        }
    }
}

impl From<SnapshotRepr> for InputSnapshot {
    fn from(s: SnapshotRepr) -> Self {
        Self {
            now_us: s.now_us,
            network: NetworkMetrics {
                rtt_us: s.rtt_us,
                loss_basis_points: s.loss_basis_points,
                send_queue_bytes: s.send_queue_bytes,
                send_queue_capacity: s.send_queue_capacity,
                sustainable_delivery_estimate_bps: s.sustainable_delivery_estimate_bps,
            },
            encoder: EncoderMetrics {
                encode_latency_us: s.encode_latency_us,
                in_flight_surfaces: s.in_flight_surfaces,
            },
            decoder: DecoderMetrics {
                load: s.decoder_load.map(Into::into),
            },
            presentation: PresentationMetrics {
                is_visible: s.is_visible,
                presentation_latency_us: s.presentation_latency_us,
            },
            application_limited: s.application_limited,
            route_changed: s.route_changed,
            reconnected: s.reconnected,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DecisionReasonRepr {
    Hold,
    NetworkBackoff {
        rtt_inflation: bool,
        packet_loss: bool,
        send_congested: bool,
    },
    EncoderThrottle {
        latency_exceeded: bool,
        queue_backlog: bool,
    },
    DecoderThrottle {
        picture_backlog: bool,
        excessive_work: bool,
    },
    PresentationThrottle {
        hidden: bool,
        stalled: bool,
    },
    HeadroomProbe,
    ApplicationLimitedHold,
    ConservativeReset {
        route_changed: bool,
        reconnected: bool,
    },
    SampleGap,
}

impl From<DecisionReason> for DecisionReasonRepr {
    fn from(r: DecisionReason) -> Self {
        match r {
            DecisionReason::Hold => Self::Hold,
            DecisionReason::NetworkBackoff {
                rtt_inflation,
                packet_loss,
                send_congested,
            } => Self::NetworkBackoff {
                rtt_inflation,
                packet_loss,
                send_congested,
            },
            DecisionReason::EncoderThrottle {
                latency_exceeded,
                queue_backlog,
            } => Self::EncoderThrottle {
                latency_exceeded,
                queue_backlog,
            },
            DecisionReason::DecoderThrottle {
                picture_backlog,
                excessive_work,
            } => Self::DecoderThrottle {
                picture_backlog,
                excessive_work,
            },
            DecisionReason::PresentationThrottle { hidden, stalled } => {
                Self::PresentationThrottle { hidden, stalled }
            }
            DecisionReason::HeadroomProbe => Self::HeadroomProbe,
            DecisionReason::ApplicationLimitedHold => Self::ApplicationLimitedHold,
            DecisionReason::ConservativeReset {
                route_changed,
                reconnected,
            } => Self::ConservativeReset {
                route_changed,
                reconnected,
            },
            DecisionReason::SampleGap => Self::SampleGap,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatingPointRepr {
    pub generation: u64,
    pub profile: ProfileRepr,
    pub target_bitrate_bps: u64,
    pub frame_interval_us: u64,
    pub video_bitrate_bps: u64,
    pub aggregate_ceiling_bps: u64,
}

impl From<OperatingPoint> for OperatingPointRepr {
    fn from(p: OperatingPoint) -> Self {
        Self {
            generation: p.generation,
            profile: p.profile.into(),
            target_bitrate_bps: p.target_bitrate_bps,
            frame_interval_us: p.frame_interval_us,
            video_bitrate_bps: p.budget.video_bitrate_bps,
            aggregate_ceiling_bps: p.aggregate_ceiling_bps,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum DiagnosticEventKind {
    SessionStarted {
        protocol_version: u16,
        transport: TransportKind,
        path: NetworkPathKind,
        codec_backend: CodecPathKind,
        capture_width: u32,
        capture_height: u32,
        target_fps: u16,
    },
    PathChanged {
        from: NetworkPathKind,
        to: NetworkPathKind,
        rtt_us: Option<u64>,
    },
    StageTiming {
        capture_us: Option<u64>,
        gpu_copy_us: Option<u64>,
        encode_us: Option<u64>,
        network_rtt_us: Option<u64>,
        decode_us: Option<u64>,
        present_us: Option<u64>,
        source_age_us: Option<u64>,
        copy_mode: SurfaceCopyMode,
    },
    ControllerStep {
        snapshot: SnapshotRepr,
        previous: OperatingPointRepr,
        current: OperatingPointRepr,
        reason: DecisionReasonRepr,
    },
    Recovery {
        kind: RecoveryEventKind,
        frame_id: u64,
        recovery_bytes: u64,
    },
    QueueHighWater {
        stage: QueueStage,
        count: u32,
        capacity: u32,
        bytes: u64,
        byte_capacity: u64,
        dropped: u64,
    },
    SanitizedNote {
        category: NoteCategory,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticEvent {
    pub sequence: u64,
    pub at_us: u64,
    pub kind: DiagnosticEventKind,
}

impl DiagnosticEvent {
    fn estimated_bytes(&self) -> usize {
        let base = std::mem::size_of::<Self>();
        let extra = match &self.kind {
            DiagnosticEventKind::SanitizedNote { message, .. } => message.len(),
            _ => 0,
        };
        base.saturating_add(extra)
    }

    fn sanitized(self) -> Self {
        match self.kind {
            DiagnosticEventKind::SanitizedNote { category, message } => Self {
                sequence: self.sequence,
                at_us: self.at_us,
                kind: DiagnosticEventKind::SanitizedNote {
                    category,
                    message: sanitize_diagnostic_text(&message),
                },
            },
            other => Self {
                sequence: self.sequence,
                at_us: self.at_us,
                kind: other,
            },
        }
    }
}

/// Scrub hostnames, IPv4/IPv6 addresses, filesystem paths, email addresses,
/// PEM/key markers, secrets/nonces/tokens, and quoted window titles.
#[must_use]
pub fn sanitize_diagnostic_text(raw: &str) -> String {
    if raw.contains("-----BEGIN") || raw.contains("PRIVATE KEY") || raw.contains("CERTIFICATE") {
        return "[redacted:pem]".to_string();
    }

    // First scrub quoted segments (e.g., window titles or filenames in library errors).
    let mut no_quotes = String::with_capacity(raw.len().min(MAX_NOTE_BYTES));
    let mut in_quote = false;
    for ch in raw.chars() {
        if ch == '"' || ch == '\'' {
            if !in_quote {
                no_quotes.push_str("[redacted:quoted]");
            }
            in_quote = !in_quote;
        } else if !in_quote {
            no_quotes.push(ch);
        }
    }

    let mut out = String::with_capacity(no_quotes.len().min(MAX_NOTE_BYTES));
    for (idx, token) in no_quotes.split_whitespace().enumerate() {
        if idx > 0 {
            out.push(' ');
        }
        let cleaned = sanitize_token(token);
        out.push_str(&cleaned);
        if out.len() >= MAX_NOTE_BYTES {
            break;
        }
    }

    if out.len() > MAX_NOTE_BYTES {
        let mut end = MAX_NOTE_BYTES;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    out
}

fn sanitize_token(token: &str) -> String {
    let trimmed = token.trim_matches(|c: char| matches!(c, ',' | ';' | '(' | ')' | '[' | ']'));
    let lower = trimmed.to_ascii_lowercase();

    // Key-value secrets, user identifiers, window titles, hostnames, paths.
    if let Some((k, _)) = lower.split_once('=')
        && matches!(
            k,
            "user"
                | "username"
                | "host"
                | "hostname"
                | "title"
                | "window"
                | "token"
                | "nonce"
                | "secret"
                | "password"
                | "auth"
                | "bearer"
                | "cert"
                | "fingerprint"
                | "path"
                | "clipboard"
        )
    {
        return format!("{k}=[redacted]");
    }

    // Email addresses.
    if trimmed.contains('@') && trimmed.len() >= 3 {
        return "[redacted:email]".to_string();
    }

    // Tailscale / local / qualified hostnames (with optional :port).
    let host_part = lower.split(':').next().unwrap_or(lower.as_str());
    if host_part.ends_with(".ts.net")
        || host_part.rsplit_once('.').is_some_and(|(host, tld)| {
            !host.is_empty() && matches!(tld, "local" | "internal" | "lan")
        })
    {
        return "[redacted:host]".to_string();
    }

    // Unix or Windows paths.
    if trimmed.starts_with('/')
        || trimmed.starts_with("~/")
        || trimmed.starts_with("\\\\")
        || (trimmed.len() >= 3
            && trimmed.as_bytes()[0].is_ascii_alphabetic()
            && trimmed.as_bytes()[1] == b':'
            && (trimmed.as_bytes()[2] == b'\\' || trimmed.as_bytes()[2] == b'/'))
    {
        return "[redacted:path]".to_string();
    }

    // IPv4 / IPv6 literals (with optional port).
    if looks_like_ip(trimmed) {
        return "[redacted:addr]".to_string();
    }

    // Long hex/base64-like tokens (cert fingerprints, nonces, hashes >= 24 chars).
    if trimmed.len() >= 24
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'-')
        && trimmed.bytes().filter(u8::is_ascii_hexdigit).count() >= 20
    {
        return "[redacted:token]".to_string();
    }

    token.to_string()
}

fn looks_like_ip(token: &str) -> bool {
    let raw = token.trim_matches(|c| c == '[' || c == ']');
    if raw.parse::<std::net::IpAddr>().is_ok() || token.parse::<std::net::SocketAddr>().is_ok() {
        return true;
    }
    // Catch bracketed IPv6 with port whether or not leading '[' was trimmed:
    // `[fd7a::1]:8443` or `fd7a::1]:8443`.
    if let Some((addr, _)) = raw.split_once(']')
        && addr.parse::<std::net::IpAddr>().is_ok()
    {
        return true;
    }
    // Catch IPv4:port like `100.74.254.33:8443`.
    if let Some((host, port)) = raw.rsplit_once(':')
        && !host.contains(':')
        && !port.is_empty()
        && port.bytes().all(|b| b.is_ascii_digit())
        && host.parse::<std::net::IpAddr>().is_ok()
    {
        return true;
    }
    false
}

/// Specific, actionable root causes for "why does this feel slow?" (Plan §3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlownessCause {
    RelayedTailnetPath,
    NetworkQueueGrowth,
    OverloadedEncoder,
    GpuToCpuCopies,
    DecoderBacklog,
    PresentationStall,
    ThermalThrottling,
    CaptureFreshnessStale,
    RecoveryStorm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CauseSeverity {
    Notice,
    Degraded,
    Severe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttributedCause {
    pub cause: SlownessCause,
    pub severity: CauseSeverity,
    pub evidence_samples: u32,
    pub peak_metric_us: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlownessVerdict {
    /// Not enough stage or controller samples to attribute latency honestly.
    InsufficientEvidence,
    /// Evidence is present and every measured stage stayed within healthy bounds.
    Healthy,
    /// One or more concrete bottlenecks were observed in retained evidence.
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlownessReport {
    pub verdict: SlownessVerdict,
    pub primary_cause: Option<SlownessCause>,
    pub causes: Vec<AttributedCause>,
    pub stage_samples: u32,
    pub controller_steps: u32,
    pub observed_path: NetworkPathKind,
    pub observed_transport: Option<TransportKind>,
}

/// Bounded ring buffer retaining sanitized diagnostic events and controller inputs.
#[derive(Debug)]
pub struct DiagnosticRing {
    limits: DiagnosticLimits,
    policy: QualityPolicy,
    events: VecDeque<DiagnosticEvent>,
    retained_bytes: usize,
    next_sequence: u64,
    evicted_events: u64,
    last_now_us: Option<u64>,
    export_timestamps_us: VecDeque<u64>,
}

impl DiagnosticRing {
    pub fn new(policy: QualityPolicy, limits: DiagnosticLimits) -> Result<Self, DiagnosticError> {
        policy
            .validate()
            .map_err(|_| DiagnosticError::InvalidPolicy)?;
        limits.validate()?;
        Ok(Self {
            limits,
            policy,
            events: VecDeque::new(),
            retained_bytes: 0,
            next_sequence: 1,
            evicted_events: 0,
            last_now_us: None,
            export_timestamps_us: VecDeque::new(),
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    #[must_use]
    pub const fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    #[must_use]
    pub const fn evicted_events(&self) -> u64 {
        self.evicted_events
    }

    /// Record a diagnostic event while strictly enforcing monotonic timestamps,
    /// count/byte ceilings, and immediate note sanitization.
    pub fn record(
        &mut self,
        at_us: u64,
        kind: DiagnosticEventKind,
    ) -> Result<u64, DiagnosticError> {
        if self.last_now_us.is_some_and(|prev| at_us < prev) {
            return Err(DiagnosticError::ClockRegression);
        }
        let seq = self.next_sequence;
        let event = DiagnosticEvent {
            sequence: seq,
            at_us,
            kind,
        }
        .sanitized();
        let bytes = event.estimated_bytes();
        if bytes > self.limits.max_bytes {
            return Err(DiagnosticError::EventTooLarge);
        }

        while self.events.len() >= self.limits.max_events
            || self.retained_bytes.saturating_add(bytes) > self.limits.max_bytes
        {
            if let Some(old) = self.events.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(old.estimated_bytes());
                self.evicted_events = self.evicted_events.saturating_add(1);
            } else {
                break;
            }
        }

        self.retained_bytes = self.retained_bytes.saturating_add(bytes);
        self.events.push_back(event);
        self.next_sequence = self.next_sequence.saturating_add(1);
        self.last_now_us = Some(at_us);
        Ok(seq)
    }

    /// Record a production [`QualityController`] decision directly for later
    /// deterministic lab replay.
    pub fn record_controller_decision(
        &mut self,
        decision: &QualityDecision,
    ) -> Result<u64, DiagnosticError> {
        self.record(
            decision.snapshot.now_us,
            DiagnosticEventKind::ControllerStep {
                snapshot: decision.snapshot.into(),
                previous: decision.previous_point.into(),
                current: decision.current_point.into(),
                reason: decision.reason.into(),
            },
        )
    }

    /// Answer "why does this feel slow?" from retained evidence alone.
    #[must_use]
    pub fn diagnose_slowness(&self) -> SlownessReport {
        diagnose_events(self.events.iter())
    }

    /// Export a rate-limited, sanitized diagnostic bundle suitable for disk export
    /// and deterministic replay in `fr-lab`.
    pub fn export_bundle(
        &mut self,
        now_us: u64,
        protocol_limits: &ProtocolLimits,
    ) -> Result<DiagnosticBundle, DiagnosticError> {
        let cutoff = now_us.saturating_sub(EXPORT_WINDOW_US);
        while self
            .export_timestamps_us
            .front()
            .is_some_and(|&ts| ts <= cutoff)
        {
            self.export_timestamps_us.pop_front();
        }
        let max_per_min =
            usize::try_from(protocol_limits.max_diagnostic_exports_per_min()).unwrap_or(usize::MAX);
        if self.export_timestamps_us.len() >= max_per_min {
            return Err(DiagnosticError::ExportRateLimited);
        }
        self.export_timestamps_us.push_back(now_us);

        let events: Vec<DiagnosticEvent> = self
            .events
            .iter()
            .cloned()
            .map(DiagnosticEvent::sanitized)
            .collect();
        let slowness = diagnose_events(events.iter());
        Ok(DiagnosticBundle {
            schema_version: BUNDLE_SCHEMA_VERSION,
            policy: self.policy.into(),
            evicted_events: self.evicted_events,
            slowness,
            events,
        })
    }
}

/// Exportable, sanitized diagnostic bundle with replayable controller trace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiagnosticBundle {
    pub schema_version: u16,
    pub policy: PolicyRepr,
    pub evicted_events: u64,
    pub slowness: SlownessReport,
    pub events: Vec<DiagnosticEvent>,
}

/// Outcome of replaying a sanitized bundle against production [`QualityController`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayMismatch {
    pub step_index: usize,
    pub expected_bitrate_bps: u64,
    pub replayed_bitrate_bps: u64,
    pub expected_interval_us: u64,
    pub replayed_interval_us: u64,
    pub expected_reason: DecisionReasonRepr,
    pub replayed_reason: DecisionReasonRepr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    pub steps_replayed: usize,
    pub matched_steps: usize,
    pub first_mismatch: Option<ReplayMismatch>,
}

impl ReplayReport {
    #[must_use]
    pub const fn is_exact_match(&self) -> bool {
        self.steps_replayed > 0
            && self.matched_steps == self.steps_replayed
            && self.first_mismatch.is_none()
    }
}

impl DiagnosticBundle {
    pub fn to_sanitized_json(&self) -> Result<String, DiagnosticError> {
        let mut clean = self.clone();
        clean.events = clean
            .events
            .into_iter()
            .map(DiagnosticEvent::sanitized)
            .collect();
        clean.slowness = diagnose_events(clean.events.iter());
        serde_json::to_string_pretty(&clean).map_err(|_| DiagnosticError::SerializationFailed)
    }

    pub fn from_json(json: &str) -> Result<Self, DiagnosticError> {
        if json.len() > MAX_IMPORT_JSON_BYTES {
            return Err(DiagnosticError::BundleTooLarge);
        }
        let bundle: Self =
            serde_json::from_str(json).map_err(|_| DiagnosticError::DeserializationFailed)?;
        if bundle.schema_version != BUNDLE_SCHEMA_VERSION {
            return Err(DiagnosticError::UnsupportedSchemaVersion(
                bundle.schema_version,
            ));
        }
        let policy: QualityPolicy = bundle.policy.into();
        policy
            .validate()
            .map_err(|_| DiagnosticError::InvalidPolicy)?;
        Ok(bundle)
    }

    /// Re-run every recorded [`DiagnosticEventKind::ControllerStep`] through a fresh
    /// production [`QualityController`] and compare every resulting operating point
    /// and decision reason bit-for-bit.
    pub fn replay_quality_controller(&self) -> Result<ReplayReport, DiagnosticError> {
        let policy: QualityPolicy = self.policy.into();
        let mut controller =
            QualityController::new(policy).map_err(DiagnosticError::ReplayError)?;
        let mut steps_replayed = 0_usize;
        let mut matched_steps = 0_usize;
        let mut first_mismatch = None;

        for event in &self.events {
            if let DiagnosticEventKind::ControllerStep {
                snapshot,
                current,
                reason,
                ..
            } = &event.kind
            {
                let input: InputSnapshot = (*snapshot).into();
                let replayed = controller
                    .update(input)
                    .map_err(DiagnosticError::ReplayError)?;
                let replayed_point: OperatingPointRepr = replayed.current_point.into();
                let replayed_reason: DecisionReasonRepr = replayed.reason.into();
                let matched = replayed_point == *current && replayed_reason == *reason;
                if matched {
                    matched_steps += 1;
                } else if first_mismatch.is_none() {
                    first_mismatch = Some(ReplayMismatch {
                        step_index: steps_replayed,
                        expected_bitrate_bps: current.target_bitrate_bps,
                        replayed_bitrate_bps: replayed_point.target_bitrate_bps,
                        expected_interval_us: current.frame_interval_us,
                        replayed_interval_us: replayed_point.frame_interval_us,
                        expected_reason: *reason,
                        replayed_reason,
                    });
                }
                steps_replayed += 1;
            }
        }

        Ok(ReplayReport {
            steps_replayed,
            matched_steps,
            first_mismatch,
        })
    }
}

#[allow(clippy::too_many_lines)]
fn diagnose_events<'a>(events: impl Iterator<Item = &'a DiagnosticEvent>) -> SlownessReport {
    let mut path = NetworkPathKind::Unknown;
    let mut transport = None;
    let mut stage_samples = 0_u32;
    let mut controller_steps = 0_u32;

    let mut relay_samples = 0_u32;
    let mut relay_peak_rtt = None;
    let mut net_queue_samples = 0_u32;
    let mut net_peak_rtt = None;
    let mut encoder_samples = 0_u32;
    let mut encoder_peak = None;
    let mut copy_samples = 0_u32;
    let mut copy_peak = None;
    let mut decoder_samples = 0_u32;
    let mut decoder_peak = None;
    let mut present_samples = 0_u32;
    let mut present_peak = None;
    let mut freshness_stale_samples = 0_u32;
    let mut freshness_stale_peak = None;
    let mut idr_recoveries = 0_u32;

    let mut encode_series: Vec<u64> = Vec::new();
    let mut decode_series: Vec<u64> = Vec::new();

    for ev in events {
        match &ev.kind {
            DiagnosticEventKind::SessionStarted {
                transport: t,
                path: p,
                ..
            } => {
                transport = Some(*t);
                path = *p;
            }
            DiagnosticEventKind::PathChanged { to, rtt_us, .. } => {
                path = *to;
                if *to == NetworkPathKind::RelayedDerp {
                    relay_samples += 1;
                    if let Some(rtt) = rtt_us {
                        relay_peak_rtt = Some(relay_peak_rtt.map_or(*rtt, |p: u64| p.max(*rtt)));
                    }
                }
            }
            DiagnosticEventKind::StageTiming {
                gpu_copy_us,
                encode_us,
                network_rtt_us,
                decode_us,
                present_us,
                source_age_us,
                copy_mode,
                ..
            } => {
                stage_samples += 1;
                if path == NetworkPathKind::RelayedDerp
                    && network_rtt_us.is_some_and(|rtt| rtt >= 40_000)
                {
                    relay_samples += 1;
                    if let Some(rtt) = network_rtt_us {
                        relay_peak_rtt = Some(relay_peak_rtt.map_or(*rtt, |p: u64| p.max(*rtt)));
                    }
                }
                if *copy_mode == SurfaceCopyMode::CpuReadback
                    && gpu_copy_us.is_some_and(|c| c >= 4_000)
                {
                    copy_samples += 1;
                    let c = gpu_copy_us.unwrap_or(0);
                    copy_peak = Some(copy_peak.map_or(c, |p: u64| p.max(c)));
                }
                if let Some(enc) = encode_us {
                    encode_series.push(*enc);
                    if *enc >= 16_666 {
                        encoder_samples += 1;
                        encoder_peak = Some(encoder_peak.map_or(*enc, |p: u64| p.max(*enc)));
                    }
                }
                if let Some(dec) = decode_us {
                    decode_series.push(*dec);
                    if *dec >= 16_666 {
                        decoder_samples += 1;
                        decoder_peak = Some(decoder_peak.map_or(*dec, |p: u64| p.max(*dec)));
                    }
                }
                if let Some(pres) = present_us
                    && *pres >= 50_000
                {
                    present_samples += 1;
                    present_peak = Some(present_peak.map_or(*pres, |p: u64| p.max(*pres)));
                }
                if let Some(age) = source_age_us
                    && *age >= 150_000
                {
                    freshness_stale_samples += 1;
                    freshness_stale_peak =
                        Some(freshness_stale_peak.map_or(*age, |p: u64| p.max(*age)));
                }
            }
            DiagnosticEventKind::ControllerStep {
                snapshot, reason, ..
            } => {
                controller_steps += 1;
                if let Some(enc) = snapshot.encode_latency_us {
                    encode_series.push(enc);
                }
                if let Some(work) = snapshot.decoder_load.and_then(|l| l.work_us) {
                    decode_series.push(work);
                }
                match reason {
                    DecisionReasonRepr::NetworkBackoff { .. } => {
                        net_queue_samples += 1;
                        if let Some(rtt) = snapshot.rtt_us {
                            net_peak_rtt = Some(net_peak_rtt.map_or(rtt, |p: u64| p.max(rtt)));
                        }
                    }
                    DecisionReasonRepr::EncoderThrottle { .. } => {
                        encoder_samples += 1;
                        if let Some(enc) = snapshot.encode_latency_us {
                            encoder_peak = Some(encoder_peak.map_or(enc, |p: u64| p.max(enc)));
                        }
                    }
                    DecisionReasonRepr::DecoderThrottle { .. } => {
                        decoder_samples += 1;
                        if let Some(work) = snapshot.decoder_load.and_then(|l| l.work_us) {
                            decoder_peak = Some(decoder_peak.map_or(work, |p: u64| p.max(work)));
                        }
                    }
                    DecisionReasonRepr::PresentationThrottle { .. } => {
                        present_samples += 1;
                        if let Some(pres) = snapshot.presentation_latency_us {
                            present_peak = Some(present_peak.map_or(pres, |p: u64| p.max(pres)));
                        }
                    }
                    _ => {}
                }
            }
            DiagnosticEventKind::QueueHighWater {
                stage,
                count,
                capacity,
                dropped,
                ..
            } => {
                if *stage == QueueStage::TransportSend
                    && (*dropped > 0 || (*capacity > 0 && *count * 4 >= *capacity * 3))
                {
                    net_queue_samples += 1;
                }
            }
            DiagnosticEventKind::Recovery {
                kind: RecoveryEventKind::ReliableIdrRecovery,
                ..
            } => {
                idr_recoveries += 1;
            }
            DiagnosticEventKind::Recovery { .. } | DiagnosticEventKind::SanitizedNote { .. } => {}
        }
    }

    if stage_samples == 0 && controller_steps == 0 && relay_samples == 0 && idr_recoveries == 0 {
        return SlownessReport {
            verdict: SlownessVerdict::InsufficientEvidence,
            primary_cause: None,
            causes: Vec::new(),
            stage_samples,
            controller_steps,
            observed_path: path,
            observed_transport: transport,
        };
    }

    let mut causes = Vec::new();
    let push_if = |causes: &mut Vec<AttributedCause>,
                   cause: SlownessCause,
                   samples: u32,
                   severe_at: u32,
                   peak: Option<u64>| {
        if samples > 0 {
            let severity = if samples >= severe_at {
                CauseSeverity::Severe
            } else if samples >= 2 {
                CauseSeverity::Degraded
            } else {
                CauseSeverity::Notice
            };
            causes.push(AttributedCause {
                cause,
                severity,
                evidence_samples: samples,
                peak_metric_us: peak,
            });
        }
    };

    push_if(
        &mut causes,
        SlownessCause::NetworkQueueGrowth,
        net_queue_samples,
        3,
        net_peak_rtt,
    );
    push_if(
        &mut causes,
        SlownessCause::RelayedTailnetPath,
        relay_samples,
        3,
        relay_peak_rtt,
    );
    push_if(
        &mut causes,
        SlownessCause::OverloadedEncoder,
        encoder_samples,
        3,
        encoder_peak,
    );
    push_if(
        &mut causes,
        SlownessCause::GpuToCpuCopies,
        copy_samples,
        3,
        copy_peak,
    );
    push_if(
        &mut causes,
        SlownessCause::DecoderBacklog,
        decoder_samples,
        3,
        decoder_peak,
    );
    push_if(
        &mut causes,
        SlownessCause::PresentationStall,
        present_samples,
        3,
        present_peak,
    );
    push_if(
        &mut causes,
        SlownessCause::CaptureFreshnessStale,
        freshness_stale_samples,
        2,
        freshness_stale_peak,
    );
    if idr_recoveries >= 3 {
        causes.push(AttributedCause {
            cause: SlownessCause::RecoveryStorm,
            severity: CauseSeverity::Severe,
            evidence_samples: idr_recoveries,
            peak_metric_us: None,
        });
    }
    if let Some((samples, peak)) =
        detect_thermal_drift(&encode_series).or_else(|| detect_thermal_drift(&decode_series))
    {
        causes.push(AttributedCause {
            cause: SlownessCause::ThermalThrottling,
            severity: CauseSeverity::Degraded,
            evidence_samples: samples,
            peak_metric_us: Some(peak),
        });
    }

    causes.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| b.evidence_samples.cmp(&a.evidence_samples))
    });

    let primary_cause = causes.first().map(|c| c.cause);
    let verdict = if causes.is_empty() {
        SlownessVerdict::Healthy
    } else {
        SlownessVerdict::Degraded
    };

    SlownessReport {
        verdict,
        primary_cause,
        causes,
        stage_samples,
        controller_steps,
        observed_path: path,
        observed_transport: transport,
    }
}

/// Detect sustained >= 50% latency drift between early and late steady samples
/// (requires at least 6 samples and late mean >= 12 ms to avoid low-latency noise).
fn detect_thermal_drift(series: &[u64]) -> Option<(u32, u64)> {
    if series.len() < 6 {
        return None;
    }
    let mid = series.len() / 2;
    let first_avg = series[..mid].iter().copied().sum::<u64>() / (mid as u64);
    let second_len = (series.len() - mid) as u64;
    let second_avg = series[mid..].iter().copied().sum::<u64>() / second_len;
    if first_avg > 0 && second_avg >= 12_000 && second_avg * 2 >= first_avg * 3 {
        let peak = series[mid..].iter().copied().max().unwrap_or(second_avg);
        let count = u32::try_from(series.len() - mid).unwrap_or(u32::MAX);
        Some((count, peak))
    } else {
        None
    }
}
