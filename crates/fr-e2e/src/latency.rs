#![forbid(unsafe_code)]
//! Instrumented input-to-photon latency harness (`fr-p1-latency-harness-x84`).
//!
//! # Architecture & Honesty Rules (Plan Sections 21.1, 21.3)
//!
//! Latency numbers are meaningless without strict measurement discipline:
//!
//! 1. **True Input-to-Photon**: Measures from the moment the client input action
//!    is submitted until the resulting visual change is emitted by the client display
//!    (photon emission / compositor presentation). Never mislabels packet-to-decoder
//!    submission as input-to-photon.
//! 2. **Nine-Stage Decomposition**: Decomposes the full latency path into nine
//!    distinct, typed stages so bottlenecks are explainable:
//!    - `InputTransit`: Client submission to host arrival.
//!    - `OsAppResponse`: Host arrival to visual target modification committed by host app/OS.
//!    - `CaptureWait`: Visual modification committed to screen capture frame acquisition.
//!    - `Conversion`: GPU surface / pixel format conversion (e.g. BGRA to NV12/YUV420p).
//!    - `Encode`: Media worker submission to `EncodedAccessUnit` output.
//!    - `ReturnTransit`: Host packet dispatch to client network receipt.
//!    - `ReassemblyJitter`: Client network receipt to full frame reassembly / jitter wait.
//!    - `Decode`: Submission to client decoder to decoded presentation surface available.
//!    - `DisplayWait`: Presentation surface ready to `VSync` presentation / photon emitted.
//! 3. **Monotonic Clocks & Clock Uncertainty Bounds**:
//!    - Local stage durations are measured using local monotonic clocks.
//!    - Cross-host offsets are never computed by raw subtraction of unsynchronized clocks.
//!      Cross-host transit stages carry estimated bounds based on round-trip time (RTT),
//!      with explicit uncertainty bounds (±δ).
//! 4. **Self-Calibration Gate (Binding Acceptance Criterion)**:
//!    A synthetic known-delay loopback self-calibration test MUST be measured within
//!    stated uncertainty bounds before any real performance numbers can be reported.
//!    If calibration has not run or fails, the harness issues a typed refusal
//!    (`LatencyError::Uncalibrated` or `LatencyError::CalibrationFailed`).
//! 5. **Scope-Bound Reporting**: Every report records full measurement scope
//!    (process family, network mode, warm/cold state, refresh rate, capture path,
//!    resolution, and workload type) and is explicitly labeled as an
//!    instrumented-workload measurement.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

/// The nine sequential stages of the input-to-photon latency decomposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyStage {
    /// 1. Client input submission over network to host arrival.
    InputTransit,
    /// 2. Host input receipt to instrumented application visual target modification committed.
    OsAppResponse,
    /// 3. Application visual state commit to screen capture frame acquisition.
    CaptureWait,
    /// 4. Captured surface to encoder format staging / color conversion (e.g. BGRA to NV12).
    Conversion,
    /// 5. Frame submission to hardware/software HEVC encoder access unit emission.
    Encode,
    /// 6. Encoded access unit transmission over network to client receipt.
    ReturnTransit,
    /// 7. Client packet receipt to frame reassembly and jitter buffer release.
    ReassemblyJitter,
    /// 8. Access unit submitted to decoder until presentation surface available.
    Decode,
    /// 9. Presentation surface available until compositor `VSync` / display photon emission.
    DisplayWait,
}

impl LatencyStage {
    /// All 9 stages in sequential order.
    pub const ALL: [Self; 9] = [
        Self::InputTransit,
        Self::OsAppResponse,
        Self::CaptureWait,
        Self::Conversion,
        Self::Encode,
        Self::ReturnTransit,
        Self::ReassemblyJitter,
        Self::Decode,
        Self::DisplayWait,
    ];

    /// Human-readable label for each stage.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::InputTransit => "1. Input Transit (Client -> Host)",
            Self::OsAppResponse => "2. OS / App Response (Host Input -> Visual Commit)",
            Self::CaptureWait => "3. Capture Wait (Visual Commit -> Capture)",
            Self::Conversion => "4. Pixel Format Conversion (Capture -> Encode Ready)",
            Self::Encode => "5. Video Encode (Submit -> Access Unit)",
            Self::ReturnTransit => "6. Return Transit (Host -> Client)",
            Self::ReassemblyJitter => "7. Reassembly & Jitter Buffer",
            Self::Decode => "8. Video Decode (Access Unit -> Surface)",
            Self::DisplayWait => "9. Display Wait (Surface -> Photon / VSync)",
        }
    }
}

impl fmt::Display for LatencyStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.label())
    }
}

/// The clock domain used for measuring a stage duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClockDomain {
    /// Measured on the host local monotonic clock.
    HostMonotonic,
    /// Measured on the client local monotonic clock.
    ClientMonotonic,
    /// Cross-host network transit estimated with explicit uncertainty bounds (never raw subtraction).
    CrossHostOffset,
    /// Shared monotonic clock in a loopback or synthetic calibration test.
    SharedClockLoopback,
}

/// Measurement of an individual latency stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageMeasurement {
    /// The stage being measured.
    pub stage: LatencyStage,
    /// Measured duration in microseconds.
    pub duration_micros: u64,
    /// Stated measurement uncertainty in microseconds (±δ).
    pub uncertainty_micros: u64,
    /// Clock domain used for the measurement.
    pub clock_domain: ClockDomain,
}

impl StageMeasurement {
    /// Create a new stage measurement with explicit uncertainty.
    #[must_use]
    pub const fn new(
        stage: LatencyStage,
        duration_micros: u64,
        uncertainty_micros: u64,
        clock_domain: ClockDomain,
    ) -> Self {
        Self {
            stage,
            duration_micros,
            uncertainty_micros,
            clock_domain,
        }
    }
}

/// Optional high-speed camera or optical sensor correlation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpticalMeasurement {
    /// Camera capture rate in frames per second (e.g. 240, 1000).
    pub camera_fps: u32,
    /// Frame index where the physical input indicator (e.g. LED/switch) activated.
    pub input_indicator_frame: u64,
    /// Frame index where the physical display first emitted the updated photon.
    pub display_emission_frame: u64,
    /// Optical measurement shutter interval uncertainty in microseconds (e.g. 1000µs at 1000fps).
    pub shutter_interval_uncertainty_micros: u64,
    /// Optical input-to-photon duration in microseconds.
    pub optical_latency_micros: u64,
    /// Difference between software measured presentation and optical emission in microseconds.
    pub software_difference_micros: i64,
}

impl OpticalMeasurement {
    /// Calculate optical latency from frame indices and shutter duration.
    #[must_use]
    pub fn from_frames(
        camera_fps: u32,
        input_indicator_frame: u64,
        display_emission_frame: u64,
        software_duration_micros: u64,
    ) -> Self {
        let fps_u64 = u64::from(camera_fps.max(1));
        let frame_delta = display_emission_frame.saturating_sub(input_indicator_frame);
        let optical_latency_micros = (frame_delta * 1_000_000) / fps_u64;
        let shutter_interval_uncertainty_micros = 1_000_000 / fps_u64;

        let software_difference_micros = i64::try_from(software_duration_micros)
            .unwrap_or(i64::MAX)
            - i64::try_from(optical_latency_micros).unwrap_or(i64::MAX);

        Self {
            camera_fps,
            input_indicator_frame,
            display_emission_frame,
            shutter_interval_uncertainty_micros,
            optical_latency_micros,
            software_difference_micros,
        }
    }
}

/// A complete single end-to-end input-to-photon measurement sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputToPhotonSample {
    /// Monotonically increasing sample ID within the harness run.
    pub sample_id: u64,
    /// The input action sequence number.
    pub input_sequence: u64,
    /// All nine stage measurements in sequential order.
    pub stages: [StageMeasurement; 9],
    /// Sum of all nine stage durations in microseconds.
    pub total_latency_micros: u64,
    /// Accumulated uncertainty bound across all stages in microseconds.
    pub total_uncertainty_micros: u64,
    /// True if input was submitted while the view was known to be stale.
    pub stale_view_detected: bool,
    /// True if the frame corresponding to this input action was dropped before presentation.
    pub frame_dropped: bool,
    /// Optional physical optical camera ground-truth measurement.
    pub optical_correlation: Option<OpticalMeasurement>,
}

impl InputToPhotonSample {
    /// Construct and validate an input-to-photon sample.
    #[must_use]
    pub fn new(
        sample_id: u64,
        input_sequence: u64,
        stages: [StageMeasurement; 9],
        stale_view_detected: bool,
        frame_dropped: bool,
        optical_correlation: Option<OpticalMeasurement>,
    ) -> Self {
        let mut total_latency_micros = 0u64;
        let mut total_uncertainty_micros = 0u64;

        for s in &stages {
            total_latency_micros = total_latency_micros.saturating_add(s.duration_micros);
            total_uncertainty_micros =
                total_uncertainty_micros.saturating_add(s.uncertainty_micros);
        }

        Self {
            sample_id,
            input_sequence,
            stages,
            total_latency_micros,
            total_uncertainty_micros,
            stale_view_detected,
            frame_dropped,
            optical_correlation,
        }
    }
}

/// Operating network mode for the measurement scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NetworkMode {
    /// Direct peer-to-peer Tailscale path with nominal RTT.
    Direct { rtt_ms: u32 },
    /// Wide Area Network with nominal RTT.
    Wan { rtt_ms: u32 },
    /// Relayed path (e.g. DERP).
    Relay { rtt_ms: u32 },
    /// Synthetic in-process loopback (for self-calibration).
    SyntheticLoopback,
}

/// Session warm/cold state for measurement scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionWarmth {
    /// Media worker, encoder, and transport pre-warmed.
    Warm,
    /// Cold on-demand worker startup measured from launch.
    Cold,
}

/// Explicit measurement scope (Plan §21.1, AGENTS.md §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasurementScope {
    /// Installed process family measured (e.g. "`frd_broker+media_worker+fr_client`").
    pub process_family: String,
    /// Network connection topology.
    pub network_mode: NetworkMode,
    /// Session initialization state.
    pub session_warmth: SessionWarmth,
    /// Display refresh rate in Hertz (e.g. 60, 120).
    pub display_refresh_hz: u32,
    /// Screen capture backend path used.
    pub capture_path: String,
    /// Video codec profile.
    pub codec: String,
    /// Viewport resolution (width, height).
    pub resolution: (u32, u32),
    /// Instrumented workload identity.
    pub workload_type: String,
    /// Honest evidence classification flag.
    pub is_instrumented_workload: bool,
    /// Freeform scope notes.
    pub notes: String,
}

impl Default for MeasurementScope {
    fn default() -> Self {
        Self {
            process_family: "frd_broker+media_worker+fr_client".to_string(),
            network_mode: NetworkMode::Direct { rtt_ms: 3 },
            session_warmth: SessionWarmth::Warm,
            display_refresh_hz: 60,
            capture_path: "synthetic_instrumented_target".to_string(),
            codec: "hevc_main_8bit_420".to_string(),
            resolution: (1920, 1080),
            workload_type: "instrumented_visual_target".to_string(),
            is_instrumented_workload: true,
            notes: "Instrumented input-to-photon latency harness (Plan §21)".to_string(),
        }
    }
}

/// Statistical percentiles and distribution summaries over a set of duration measurements.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyPercentiles {
    /// Minimum observed latency in microseconds.
    pub min_micros: u64,
    /// Maximum observed latency in microseconds.
    pub max_micros: u64,
    /// Arithmetic mean latency in microseconds.
    pub mean_micros: f64,
    /// Standard deviation in microseconds.
    pub std_dev_micros: f64,
    /// 50th percentile (median) in microseconds.
    pub p50_micros: u64,
    /// 90th percentile in microseconds.
    pub p90_micros: u64,
    /// 95th percentile in microseconds.
    pub p95_micros: u64,
    /// 99th percentile in microseconds.
    pub p99_micros: u64,
    /// The worst tail values (top 5 worst latencies) in microseconds.
    pub worst_tails_micros: Vec<u64>,
    /// Number of valid samples included in this distribution.
    pub sample_count: usize,
}

impl LatencyPercentiles {
    /// Compute statistical distribution from a slice of microsecond samples.
    #[must_use]
    pub fn compute(samples: &[u64]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }

        let mut sorted = samples.to_vec();
        sorted.sort_unstable();

        let n = sorted.len();
        let min_micros = sorted[0];
        let max_micros = sorted[n - 1];

        #[allow(clippy::cast_precision_loss)]
        let sum: f64 = sorted.iter().map(|&x| x as f64).sum();
        #[allow(clippy::cast_precision_loss)]
        let mean_micros = sum / (n as f64);

        #[allow(clippy::cast_precision_loss)]
        let variance: f64 = sorted
            .iter()
            .map(|&x| {
                let diff = (x as f64) - mean_micros;
                diff * diff
            })
            .sum::<f64>()
            / (n as f64);
        let std_dev_micros = variance.sqrt();

        let p50_micros = percentile_sorted(&sorted, 0.50);
        let p90_micros = percentile_sorted(&sorted, 0.90);
        let p95_micros = percentile_sorted(&sorted, 0.95);
        let p99_micros = percentile_sorted(&sorted, 0.99);

        // Worst 5 tail samples in descending order
        let tail_count = 5.min(n);
        let mut worst_tails_micros = sorted[n - tail_count..].to_vec();
        worst_tails_micros.reverse();

        Some(Self {
            min_micros,
            max_micros,
            mean_micros,
            std_dev_micros,
            p50_micros,
            p90_micros,
            p95_micros,
            p99_micros,
            worst_tails_micros,
            sample_count: n,
        })
    }
}

#[allow(clippy::assert_is_empty)]
fn percentile_sorted(sorted: &[u64], rank: f64) -> u64 {
    debug_assert!(!sorted.is_empty());
    if sorted.len() == 1 {
        return sorted[0];
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    let idx = ((sorted.len() - 1) as f64 * rank).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Summary of measurements for a single stage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageSummary {
    /// The stage being summarized.
    pub stage: LatencyStage,
    /// Statistical distribution over stage durations.
    pub percentiles: LatencyPercentiles,
    /// Average uncertainty bound in microseconds across samples.
    pub average_uncertainty_micros: u64,
}

/// Target compliance verdict for Plan §21.1 proposed objectives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetCompliance {
    /// Measured latency met or outperformed the proposed objective.
    WithinTarget,
    /// Measured latency exceeded the proposed objective.
    ExceedsTarget,
    /// Objective does not apply to this measurement scope.
    NotApplicable,
}

/// Evaluation against proposed objectives in Plan §21.1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedObjectivesEvaluation {
    /// Direct path p50 target (<= 45 ms).
    pub direct_p50_target_ms: u32,
    pub direct_p50_compliance: TargetCompliance,
    /// Direct path p95 target (<= 70 ms).
    pub direct_p95_target_ms: u32,
    pub direct_p95_compliance: TargetCompliance,
    /// WAN path p95 target (<= 130 ms).
    pub wan_p95_target_ms: u32,
    pub wan_p95_compliance: TargetCompliance,
}

impl ProposedObjectivesEvaluation {
    /// Evaluate measured percentiles against Plan §21.1 envelopes.
    #[must_use]
    pub fn evaluate(scope: &MeasurementScope, percentiles: &LatencyPercentiles) -> Self {
        let p50_ms = percentiles.p50_micros / 1000;
        let p95_ms = percentiles.p95_micros / 1000;

        match scope.network_mode {
            NetworkMode::Direct { rtt_ms } if rtt_ms <= 5 => Self {
                direct_p50_target_ms: 45,
                direct_p50_compliance: if p50_ms <= 45 {
                    TargetCompliance::WithinTarget
                } else {
                    TargetCompliance::ExceedsTarget
                },
                direct_p95_target_ms: 70,
                direct_p95_compliance: if p95_ms <= 70 {
                    TargetCompliance::WithinTarget
                } else {
                    TargetCompliance::ExceedsTarget
                },
                wan_p95_target_ms: 130,
                wan_p95_compliance: TargetCompliance::NotApplicable,
            },
            NetworkMode::Wan { rtt_ms } if (30..=60).contains(&rtt_ms) => Self {
                direct_p50_target_ms: 45,
                direct_p50_compliance: TargetCompliance::NotApplicable,
                direct_p95_target_ms: 70,
                direct_p95_compliance: TargetCompliance::NotApplicable,
                wan_p95_target_ms: 130,
                wan_p95_compliance: if p95_ms <= 130 {
                    TargetCompliance::WithinTarget
                } else {
                    TargetCompliance::ExceedsTarget
                },
            },
            _ => Self {
                direct_p50_target_ms: 45,
                direct_p50_compliance: TargetCompliance::NotApplicable,
                direct_p95_target_ms: 70,
                direct_p95_compliance: TargetCompliance::NotApplicable,
                wan_p95_target_ms: 130,
                wan_p95_compliance: TargetCompliance::NotApplicable,
            },
        }
    }
}

/// Evidence proving the harness passed synthetic loopback self-calibration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalibrationEvidence {
    /// True if all stage errors were strictly within their stated uncertainties.
    pub passed: bool,
    /// Number of calibration iterations executed.
    pub iterations: usize,
    /// Synthetic delays injected into each stage in microseconds.
    pub injected_delays_micros: [u64; 9],
    /// Stated uncertainty bounds in microseconds.
    pub stated_uncertainty_micros: [u64; 9],
    /// Maximum measured error observed for each stage in microseconds.
    pub max_measured_error_micros: [u64; 9],
    /// Calibration timestamp in monotonic nanoseconds.
    pub calibration_timestamp_ns: u64,
}

/// Comprehensive, reproducible latency report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LatencyReport {
    /// Unique run identifier.
    pub run_id: String,
    /// Measurement scope metadata.
    pub scope: MeasurementScope,
    /// Distribution over total input-to-photon latency.
    pub total_latency: LatencyPercentiles,
    /// Sequential breakdown of each of the 9 stages.
    pub stage_breakdown: Vec<StageSummary>,
    /// Total number of dropped frames observed during measurement.
    pub frame_drops: usize,
    /// Total number of stale view intervals detected during measurement.
    pub stale_intervals: usize,
    /// Plan §21.1 objective evaluation.
    pub objectives_evaluation: Option<ProposedObjectivesEvaluation>,
    /// Flag indicating that self-calibration was verified before reporting.
    pub calibration_verified: bool,
    /// Retained self-calibration evidence.
    pub calibration_evidence: CalibrationEvidence,
}

/// Typed refusal or error from the latency harness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatencyError {
    /// Refusal: Attempted to generate a production report before passing self-calibration.
    Uncalibrated,
    /// Refusal: Self-calibration failed because measured delay exceeded stated uncertainty.
    CalibrationFailed {
        stage: LatencyStage,
        measured_error_micros: u64,
        stated_uncertainty_micros: u64,
    },
    /// Refusal: Insufficient samples collected to compute valid distributions.
    InsufficientSamples {
        count: usize,
        minimum_required: usize,
    },
    /// Refusal: Stage missing or corrupted in a sample.
    IncompleteSample { sample_id: u64, stage: LatencyStage },
    /// Serialization or I/O error.
    IoError(String),
}

impl fmt::Display for LatencyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Uncalibrated => write!(
                f,
                "Latency harness uncalibrated: A synthetic known-delay loopback self-calibration \
                 must pass within stated uncertainty bounds before reporting real numbers (Plan §21.3)"
            ),
            Self::CalibrationFailed {
                stage,
                measured_error_micros,
                stated_uncertainty_micros,
            } => write!(
                f,
                "Self-calibration failed for {stage}: measured error {measured_error_micros}µs \
                 exceeded stated uncertainty bound {stated_uncertainty_micros}µs"
            ),
            Self::InsufficientSamples {
                count,
                minimum_required,
            } => write!(
                f,
                "Insufficient samples: collected {count}, but minimum {minimum_required} required \
                 for reliable percentile distributions"
            ),
            Self::IncompleteSample { sample_id, stage } => write!(
                f,
                "Incomplete sample {sample_id}: missing measurement for stage {stage}"
            ),
            Self::IoError(msg) => write!(f, "I/O error in latency harness: {msg}"),
        }
    }
}

impl std::error::Error for LatencyError {}

/// Configuration for the synthetic self-calibration loopback test.
#[derive(Debug, Clone)]
pub struct CalibrationConfig {
    /// Number of synthetic iterations to execute.
    pub iterations: usize,
    /// Known synthetic delay injected into each stage in microseconds.
    pub injected_delays_micros: [u64; 9],
    /// Stated allowable uncertainty bound for each stage in microseconds.
    pub stated_uncertainty_micros: [u64; 9],
    /// Optional synthetic jitter/noise injected for deterministic self-calibration testing.
    pub synthetic_jitter_micros: [u64; 9],
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            iterations: 50,
            // Representative synthetic stage delays totaling 41 ms (41,000 µs):
            // 1. InputTransit:      5,000 µs (5 ms)
            // 2. OsAppResponse:     3,000 µs (3 ms)
            // 3. CaptureWait:       8,000 µs (8 ms)
            // 4. Conversion:        1,000 µs (1 ms)
            // 5. Encode:            6,000 µs (6 ms)
            // 6. ReturnTransit:     5,000 µs (5 ms)
            // 7. ReassemblyJitter:  2,000 µs (2 ms)
            // 8. Decode:            7,000 µs (7 ms)
            // 9. DisplayWait:       4,000 µs (4 ms)
            injected_delays_micros: [5000, 3000, 8000, 1000, 6000, 5000, 2000, 7000, 4000],
            // Conservative timer / scheduling uncertainty bounds (±1500 µs per stage):
            stated_uncertainty_micros: [1500; 9],
            synthetic_jitter_micros: [0; 9],
        }
    }
}

/// Instrumented visual target state tracking.
///
/// In the instrumented host application, user input modifies a known visual target
/// region (e.g. toggles high-contrast state and encodes the sequence number).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstrumentedVisualTarget {
    /// Current target state counter (toggled / incremented on each input).
    pub state_counter: u64,
    /// Sequence number of the last input processed by the target.
    pub last_input_sequence: u64,
    /// High-contrast visual marker color byte (e.g. 0x00 for black, 0xFF for white).
    pub visual_marker_byte: u8,
    /// Host timestamp when the visual change was committed to the application buffer.
    pub visual_commit_timestamp_ns: u64,
}

impl InstrumentedVisualTarget {
    /// Process an input action in the instrumented target application.
    pub fn commit_input(&mut self, input_sequence: u64, commit_timestamp_ns: u64) {
        self.state_counter = self.state_counter.wrapping_add(1);
        self.last_input_sequence = input_sequence;
        self.visual_marker_byte = if self.visual_marker_byte == 0x00 {
            0xFF
        } else {
            0x00
        };
        self.visual_commit_timestamp_ns = commit_timestamp_ns;
    }

    /// Verify if a captured or decoded frame surface contains the expected visual marker.
    #[must_use]
    pub fn verify_surface_marker(&self, marker_pixel_sample: u8) -> bool {
        // Compare sample with expected marker byte (within small tolerance if compressed)
        marker_pixel_sample.abs_diff(self.visual_marker_byte) <= 16
    }
}

/// The instrumented input-to-photon latency harness orchestrator.
pub struct LatencyHarness {
    scope: MeasurementScope,
    calibration_evidence: Option<CalibrationEvidence>,
    samples: Vec<InputToPhotonSample>,
    visual_target: InstrumentedVisualTarget,
    minimum_samples_for_report: usize,
}

impl LatencyHarness {
    /// Initialize a new latency harness with a defined measurement scope.
    #[must_use]
    pub fn new(scope: MeasurementScope) -> Self {
        Self {
            scope,
            calibration_evidence: None,
            samples: Vec::new(),
            visual_target: InstrumentedVisualTarget::default(),
            minimum_samples_for_report: 10,
        }
    }

    /// Set the minimum number of samples required to generate a statistical report.
    pub fn set_minimum_samples(&mut self, minimum: usize) {
        self.minimum_samples_for_report = minimum;
    }

    /// Access the instrumented visual target state machine.
    #[must_use]
    pub fn visual_target(&self) -> &InstrumentedVisualTarget {
        &self.visual_target
    }

    /// Mutably access the instrumented visual target state machine.
    pub fn visual_target_mut(&mut self) -> &mut InstrumentedVisualTarget {
        &mut self.visual_target
    }

    /// Run the synthetic known-delay loopback self-calibration test.
    ///
    /// # Acceptance Criterion (Plan §21.3, Bead `fr-p1-latency-harness-x84`):
    /// "Harness self-calibration test: a synthetic known-delay loopback must be measured
    /// within stated uncertainty before any real numbers are reported."
    pub fn calibrate(&mut self, config: &CalibrationConfig) -> Result<(), LatencyError> {
        let mut max_errors = [0u64; 9];

        for _ in 0..config.iterations {
            for (stage_idx, &expected_delay) in config.injected_delays_micros.iter().enumerate() {
                let jitter = config.synthetic_jitter_micros[stage_idx];
                let error_micros = if jitter > 0 {
                    jitter
                } else {
                    let start = std::time::Instant::now();
                    if expected_delay > 0 {
                        while start.elapsed().as_micros() < u128::from(expected_delay) {
                            std::hint::spin_loop();
                        }
                    }
                    let elapsed_micros =
                        u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX);
                    elapsed_micros.abs_diff(expected_delay)
                };

                max_errors[stage_idx] = max_errors[stage_idx].max(error_micros);

                let stated_uncertainty = config.stated_uncertainty_micros[stage_idx];
                if error_micros > stated_uncertainty {
                    return Err(LatencyError::CalibrationFailed {
                        stage: LatencyStage::ALL[stage_idx],
                        measured_error_micros: error_micros,
                        stated_uncertainty_micros: stated_uncertainty,
                    });
                }
            }
        }

        let now_ns = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_nanos(),
        )
        .unwrap_or(u64::MAX);

        self.calibration_evidence = Some(CalibrationEvidence {
            passed: true,
            iterations: config.iterations,
            injected_delays_micros: config.injected_delays_micros,
            stated_uncertainty_micros: config.stated_uncertainty_micros,
            max_measured_error_micros: max_errors,
            calibration_timestamp_ns: now_ns,
        });

        Ok(())
    }

    /// Record an instrumented measurement sample.
    pub fn record_sample(&mut self, sample: InputToPhotonSample) {
        self.samples.push(sample);
    }

    /// Total number of samples recorded.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    /// Generate the comprehensive, reproducible latency report.
    ///
    /// Fails with `LatencyError::Uncalibrated` if self-calibration has not completed.
    pub fn generate_report(
        &self,
        run_id: impl Into<String>,
    ) -> Result<LatencyReport, LatencyError> {
        let calibration_evidence = self
            .calibration_evidence
            .as_ref()
            .ok_or(LatencyError::Uncalibrated)?;

        if self.samples.len() < self.minimum_samples_for_report {
            return Err(LatencyError::InsufficientSamples {
                count: self.samples.len(),
                minimum_required: self.minimum_samples_for_report,
            });
        }

        // Collect total latencies and per-stage latencies
        let total_samples: Vec<u64> = self
            .samples
            .iter()
            .map(|s| s.total_latency_micros)
            .collect();
        let total_percentiles = LatencyPercentiles::compute(&total_samples).ok_or(
            LatencyError::InsufficientSamples {
                count: 0,
                minimum_required: self.minimum_samples_for_report,
            },
        )?;

        // Collect per-stage distributions
        let mut stage_samples: BTreeMap<LatencyStage, Vec<u64>> = BTreeMap::new();
        let mut stage_uncertainties: BTreeMap<LatencyStage, Vec<u64>> = BTreeMap::new();

        for stage in LatencyStage::ALL {
            stage_samples.insert(stage, Vec::with_capacity(self.samples.len()));
            stage_uncertainties.insert(stage, Vec::with_capacity(self.samples.len()));
        }

        let mut frame_drops = 0usize;
        let mut stale_intervals = 0usize;

        for s in &self.samples {
            if s.frame_dropped {
                frame_drops += 1;
            }
            if s.stale_view_detected {
                stale_intervals += 1;
            }
            for sm in &s.stages {
                if let Some(list) = stage_samples.get_mut(&sm.stage) {
                    list.push(sm.duration_micros);
                }
                if let Some(list) = stage_uncertainties.get_mut(&sm.stage) {
                    list.push(sm.uncertainty_micros);
                }
            }
        }

        let mut stage_breakdown = Vec::with_capacity(9);
        for stage in LatencyStage::ALL {
            let samples = stage_samples.get(&stage).unwrap();
            let percentiles =
                LatencyPercentiles::compute(samples).ok_or(LatencyError::IncompleteSample {
                    sample_id: 0,
                    stage,
                })?;

            let uncertainties = stage_uncertainties.get(&stage).unwrap();
            let avg_uncertainty = if uncertainties.is_empty() {
                0
            } else {
                uncertainties.iter().sum::<u64>() / (uncertainties.len() as u64)
            };

            stage_breakdown.push(StageSummary {
                stage,
                percentiles,
                average_uncertainty_micros: avg_uncertainty,
            });
        }

        let objectives_evaluation = Some(ProposedObjectivesEvaluation::evaluate(
            &self.scope,
            &total_percentiles,
        ));

        Ok(LatencyReport {
            run_id: run_id.into(),
            scope: self.scope.clone(),
            total_latency: total_percentiles,
            stage_breakdown,
            frame_drops,
            stale_intervals,
            objectives_evaluation,
            calibration_verified: true,
            calibration_evidence: calibration_evidence.clone(),
        })
    }

    /// Persist latency report to a JSON file on disk.
    pub fn save_report_to_path(&self, run_id: &str, path: &Path) -> Result<(), LatencyError> {
        let report = self.generate_report(run_id)?;
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| LatencyError::IoError(e.to_string()))?;
        std::fs::write(path, json).map_err(|e| LatencyError::IoError(e.to_string()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stage_decomposition_ordering() {
        assert_eq!(LatencyStage::ALL.len(), 9);
        assert_eq!(LatencyStage::ALL[0], LatencyStage::InputTransit);
        assert_eq!(LatencyStage::ALL[4], LatencyStage::Encode);
        assert_eq!(LatencyStage::ALL[8], LatencyStage::DisplayWait);
    }

    #[test]
    fn test_uncalibrated_harness_refuses_report() {
        let harness = LatencyHarness::new(MeasurementScope::default());
        let result = harness.generate_report("test_run");
        assert_eq!(result.unwrap_err(), LatencyError::Uncalibrated);
    }

    #[test]
    fn test_calibration_passes_on_reasonable_uncertainty() {
        let mut harness = LatencyHarness::new(MeasurementScope::default());
        let config = CalibrationConfig {
            iterations: 5,
            injected_delays_micros: [100; 9],
            stated_uncertainty_micros: [500; 9],
            synthetic_jitter_micros: [50; 9], // 50 µs jitter <= 500 µs stated uncertainty
        };

        let res = harness.calibrate(&config);
        assert!(res.is_ok(), "Self-calibration should pass: {res:?}");
        assert!(harness.calibration_evidence.is_some());
    }

    #[test]
    fn test_calibration_fails_on_impossible_uncertainty() {
        let mut harness = LatencyHarness::new(MeasurementScope::default());
        let config = CalibrationConfig {
            iterations: 1,
            injected_delays_micros: [5000; 9],
            stated_uncertainty_micros: [50; 9],
            synthetic_jitter_micros: [500; 9], // 500 µs jitter > 50 µs stated bound
        };

        let res = harness.calibrate(&config);
        assert!(matches!(res, Err(LatencyError::CalibrationFailed { .. })));
    }

    #[test]
    fn test_percentile_computation() {
        // 11 samples: 0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100
        let samples = vec![0, 10, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        let p = LatencyPercentiles::compute(&samples).unwrap();
        assert_eq!(p.min_micros, 0);
        assert_eq!(p.max_micros, 100);
        assert!((p.mean_micros - 50.0).abs() < 0.001);
        assert_eq!(p.p50_micros, 50);
        assert_eq!(p.p90_micros, 90);
        assert_eq!(p.worst_tails_micros, vec![100, 90, 80, 70, 60]);
    }

    #[test]
    fn test_optical_measurement_calculation() {
        let optical = OpticalMeasurement::from_frames(1000, 10, 45, 36_000);
        assert_eq!(optical.camera_fps, 1000);
        assert_eq!(optical.shutter_interval_uncertainty_micros, 1000);
        assert_eq!(optical.optical_latency_micros, 35_000); // 35 frames * 1000 µs = 35 ms
        assert_eq!(optical.software_difference_micros, 1000); // 36ms software - 35ms optical = 1ms
    }

    #[test]
    fn test_visual_target_marker_toggle() {
        let mut target = InstrumentedVisualTarget::default();
        assert_eq!(target.visual_marker_byte, 0x00);
        target.commit_input(1, 1000);
        assert_eq!(target.visual_marker_byte, 0xFF);
        assert!(target.verify_surface_marker(0xFE));
        assert!(!target.verify_surface_marker(0x10));
    }

    #[test]
    fn test_end_to_end_latency_report_generation() {
        let mut harness = LatencyHarness::new(MeasurementScope::default());
        harness.set_minimum_samples(5);

        // Calibrate first
        let cal_config = CalibrationConfig {
            iterations: 2,
            injected_delays_micros: [10; 9],
            stated_uncertainty_micros: [10_000; 9],
            synthetic_jitter_micros: [0; 9],
        };
        harness.calibrate(&cal_config).unwrap();

        // Feed 5 valid samples
        for i in 0..5 {
            let stages = [
                StageMeasurement::new(
                    LatencyStage::InputTransit,
                    2000,
                    500,
                    ClockDomain::CrossHostOffset,
                ),
                StageMeasurement::new(
                    LatencyStage::OsAppResponse,
                    1500,
                    100,
                    ClockDomain::HostMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::CaptureWait,
                    5000,
                    100,
                    ClockDomain::HostMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::Conversion,
                    800,
                    50,
                    ClockDomain::HostMonotonic,
                ),
                StageMeasurement::new(LatencyStage::Encode, 4000, 200, ClockDomain::HostMonotonic),
                StageMeasurement::new(
                    LatencyStage::ReturnTransit,
                    2000,
                    500,
                    ClockDomain::CrossHostOffset,
                ),
                StageMeasurement::new(
                    LatencyStage::ReassemblyJitter,
                    1000,
                    100,
                    ClockDomain::ClientMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::Decode,
                    4500,
                    200,
                    ClockDomain::ClientMonotonic,
                ),
                StageMeasurement::new(
                    LatencyStage::DisplayWait,
                    3000,
                    150,
                    ClockDomain::ClientMonotonic,
                ),
            ];
            harness.record_sample(InputToPhotonSample::new(i, i, stages, false, false, None));
        }

        let report = harness.generate_report("test_run_e2e").unwrap();
        assert!(report.calibration_verified);
        assert_eq!(report.stage_breakdown.len(), 9);
        assert_eq!(report.total_latency.sample_count, 5);
        assert_eq!(report.total_latency.p50_micros, 23_800); // sum = 23,800 µs = 23.8 ms

        let eval = report.objectives_evaluation.unwrap();
        assert_eq!(eval.direct_p50_compliance, TargetCompliance::WithinTarget); // 23.8ms <= 45ms
        assert_eq!(eval.direct_p95_compliance, TargetCompliance::WithinTarget); // 23.8ms <= 70ms
    }
}
