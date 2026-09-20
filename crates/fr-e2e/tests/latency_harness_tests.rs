#![forbid(unsafe_code)]
//! Integration tests for the instrumented input-to-photon latency harness (`fr-p1-latency-harness-x84`).

use fr_e2e::latency::{
    CalibrationConfig, ClockDomain, InputToPhotonSample, InstrumentedVisualTarget, LatencyError,
    LatencyHarness, LatencyPercentiles, LatencyStage, MeasurementScope, NetworkMode,
    OpticalMeasurement, ProposedObjectivesEvaluation, SessionWarmth, StageMeasurement,
    TargetCompliance,
};
use fr_e2e::{E2eHarness, HarnessConfig};
use std::fs;
use std::path::PathBuf;

#[test]
fn test_harness_self_calibration_acceptance_criterion() {
    // Plan §21.3 / Bead fr-p1-latency-harness-x84 acceptance criterion:
    // "Harness self-calibration test: a synthetic known-delay loopback must be measured
    // within stated uncertainty before any real numbers are reported"
    let mut harness = LatencyHarness::new(MeasurementScope {
        session_warmth: SessionWarmth::Warm,
        ..MeasurementScope::default()
    });

    let config = CalibrationConfig {
        iterations: 10,
        // Injected known synthetic stage delays in microseconds:
        injected_delays_micros: [500, 300, 800, 100, 600, 500, 200, 700, 400],
        // Stated uncertainty bounds in microseconds:
        stated_uncertainty_micros: [500; 9],
        synthetic_jitter_micros: [50; 9], // 50 µs jitter <= 500 µs bound
    };

    let cal_result = harness.calibrate(&config);
    assert!(
        cal_result.is_ok(),
        "Self-calibration must succeed within stated bounds: {cal_result:?}"
    );

    // After calibration passes, report generation is unlocked (provided sample count met)
    harness.set_minimum_samples(2);
    for i in 0..2 {
        let stages = [
            StageMeasurement::new(
                LatencyStage::InputTransit,
                1000,
                200,
                ClockDomain::CrossHostOffset,
            ),
            StageMeasurement::new(
                LatencyStage::OsAppResponse,
                1000,
                50,
                ClockDomain::HostMonotonic,
            ),
            StageMeasurement::new(
                LatencyStage::CaptureWait,
                1000,
                50,
                ClockDomain::HostMonotonic,
            ),
            StageMeasurement::new(
                LatencyStage::Conversion,
                500,
                20,
                ClockDomain::HostMonotonic,
            ),
            StageMeasurement::new(LatencyStage::Encode, 2000, 100, ClockDomain::HostMonotonic),
            StageMeasurement::new(
                LatencyStage::ReturnTransit,
                1000,
                200,
                ClockDomain::CrossHostOffset,
            ),
            StageMeasurement::new(
                LatencyStage::ReassemblyJitter,
                500,
                50,
                ClockDomain::ClientMonotonic,
            ),
            StageMeasurement::new(
                LatencyStage::Decode,
                2000,
                100,
                ClockDomain::ClientMonotonic,
            ),
            StageMeasurement::new(
                LatencyStage::DisplayWait,
                1000,
                50,
                ClockDomain::ClientMonotonic,
            ),
        ];
        harness.record_sample(InputToPhotonSample::new(i, i, stages, false, false, None));
    }

    let report = harness.generate_report("run_calibrated");
    assert!(
        report.is_ok(),
        "Calibrated harness must produce report: {report:?}"
    );
    let rep = report.unwrap();
    assert!(rep.calibration_verified);
    assert!(rep.calibration_evidence.passed);
    assert_eq!(rep.calibration_evidence.iterations, 10);
}

#[test]
fn test_harness_refuses_reporting_when_uncalibrated() {
    let mut harness = LatencyHarness::new(MeasurementScope::default());
    harness.set_minimum_samples(1);

    let stages = [
        StageMeasurement::new(
            LatencyStage::InputTransit,
            1000,
            200,
            ClockDomain::CrossHostOffset,
        ),
        StageMeasurement::new(
            LatencyStage::OsAppResponse,
            1000,
            50,
            ClockDomain::HostMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::CaptureWait,
            1000,
            50,
            ClockDomain::HostMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::Conversion,
            500,
            20,
            ClockDomain::HostMonotonic,
        ),
        StageMeasurement::new(LatencyStage::Encode, 2000, 100, ClockDomain::HostMonotonic),
        StageMeasurement::new(
            LatencyStage::ReturnTransit,
            1000,
            200,
            ClockDomain::CrossHostOffset,
        ),
        StageMeasurement::new(
            LatencyStage::ReassemblyJitter,
            500,
            50,
            ClockDomain::ClientMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::Decode,
            2000,
            100,
            ClockDomain::ClientMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::DisplayWait,
            1000,
            50,
            ClockDomain::ClientMonotonic,
        ),
    ];
    harness.record_sample(InputToPhotonSample::new(1, 1, stages, false, false, None));

    let err = harness.generate_report("uncalibrated_run").unwrap_err();
    assert_eq!(
        err,
        LatencyError::Uncalibrated,
        "Uncalibrated harness must issue typed refusal"
    );
}

#[test]
fn test_harness_refuses_when_calibration_exceeds_bounds() {
    let mut harness = LatencyHarness::new(MeasurementScope::default());

    // Inject 500 µs jitter with a 50 µs uncertainty bound
    let config = CalibrationConfig {
        iterations: 1,
        injected_delays_micros: [5000; 9],
        stated_uncertainty_micros: [50; 9],
        synthetic_jitter_micros: [500; 9],
    };

    let cal_result = harness.calibrate(&config);
    assert!(
        matches!(cal_result, Err(LatencyError::CalibrationFailed { .. })),
        "Calibration must fail if error exceeds uncertainty: {cal_result:?}"
    );

    // Reporting remains locked
    assert_eq!(
        harness.generate_report("run_fail").unwrap_err(),
        LatencyError::Uncalibrated
    );
}

#[test]
fn test_nine_stage_decomposition_and_uncertainty_accumulation() {
    let stages = [
        StageMeasurement::new(
            LatencyStage::InputTransit,
            2500,
            500,
            ClockDomain::CrossHostOffset,
        ),
        StageMeasurement::new(
            LatencyStage::OsAppResponse,
            1800,
            100,
            ClockDomain::HostMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::CaptureWait,
            5500,
            150,
            ClockDomain::HostMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::Conversion,
            1200,
            50,
            ClockDomain::HostMonotonic,
        ),
        StageMeasurement::new(LatencyStage::Encode, 4500, 200, ClockDomain::HostMonotonic),
        StageMeasurement::new(
            LatencyStage::ReturnTransit,
            2500,
            500,
            ClockDomain::CrossHostOffset,
        ),
        StageMeasurement::new(
            LatencyStage::ReassemblyJitter,
            1100,
            100,
            ClockDomain::ClientMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::Decode,
            5000,
            200,
            ClockDomain::ClientMonotonic,
        ),
        StageMeasurement::new(
            LatencyStage::DisplayWait,
            3200,
            150,
            ClockDomain::ClientMonotonic,
        ),
    ];

    let sample = InputToPhotonSample::new(1, 100, stages, false, false, None);

    // Sum of stage durations: 2500+1800+5500+1200+4500+2500+1100+5000+3200 = 27,300 µs (27.3 ms)
    assert_eq!(sample.total_latency_micros, 27_300);

    // Sum of uncertainties: 500+100+150+50+200+500+100+200+150 = 1,950 µs (1.95 ms)
    assert_eq!(sample.total_uncertainty_micros, 1_950);
}

#[test]
fn test_percentiles_and_worst_tails_distribution() {
    let mut values = Vec::new();
    // 100 samples from 10,000 µs to 109,000 µs in steps of 1000 µs
    for i in 10..=109 {
        values.push(i * 1000);
    }

    let p = LatencyPercentiles::compute(&values).unwrap();
    assert_eq!(p.sample_count, 100);
    assert_eq!(p.min_micros, 10_000);
    assert_eq!(p.max_micros, 109_000);
    assert_eq!(p.p50_micros, 60_000);
    assert_eq!(p.p90_micros, 99_000);
    assert_eq!(p.p95_micros, 104_000);
    assert_eq!(p.p99_micros, 108_000);

    // Worst tails should be top 5 in descending order
    assert_eq!(
        p.worst_tails_micros,
        vec![109_000, 108_000, 107_000, 106_000, 105_000]
    );
}

#[test]
fn test_proposed_objectives_evaluation_direct_and_wan() {
    // 1. Direct path within objectives (p50 = 25 ms <= 45 ms, p95 = 55 ms <= 70 ms)
    let direct_scope = MeasurementScope {
        network_mode: NetworkMode::Direct { rtt_ms: 3 },
        ..MeasurementScope::default()
    };
    let direct_percentiles = LatencyPercentiles {
        min_micros: 15_000,
        max_micros: 65_000,
        mean_micros: 26_000.0,
        std_dev_micros: 8_000.0,
        p50_micros: 25_000,
        p90_micros: 48_000,
        p95_micros: 55_000,
        p99_micros: 62_000,
        worst_tails_micros: vec![65_000, 64_000],
        sample_count: 50,
    };
    let eval_direct = ProposedObjectivesEvaluation::evaluate(&direct_scope, &direct_percentiles);
    assert_eq!(
        eval_direct.direct_p50_compliance,
        TargetCompliance::WithinTarget
    );
    assert_eq!(
        eval_direct.direct_p95_compliance,
        TargetCompliance::WithinTarget
    );
    assert_eq!(
        eval_direct.wan_p95_compliance,
        TargetCompliance::NotApplicable
    );

    // 2. WAN path within objectives (p95 = 110 ms <= 130 ms)
    let wan_scope = MeasurementScope {
        network_mode: NetworkMode::Wan { rtt_ms: 40 },
        ..MeasurementScope::default()
    };
    let wan_percentiles = LatencyPercentiles {
        min_micros: 45_000,
        max_micros: 125_000,
        mean_micros: 65_000.0,
        std_dev_micros: 15_000.0,
        p50_micros: 60_000,
        p90_micros: 98_000,
        p95_micros: 110_000,
        p99_micros: 120_000,
        worst_tails_micros: vec![125_000],
        sample_count: 50,
    };
    let eval_wan = ProposedObjectivesEvaluation::evaluate(&wan_scope, &wan_percentiles);
    assert_eq!(eval_wan.wan_p95_compliance, TargetCompliance::WithinTarget);
    assert_eq!(
        eval_wan.direct_p50_compliance,
        TargetCompliance::NotApplicable
    );
}

#[test]
fn test_optical_measurement_drift_correlation() {
    // High-speed camera running at 1000 fps (1 ms per frame)
    // Input indicator on at frame 100, display emission at frame 142 -> 42 ms optical latency
    let optical = OpticalMeasurement::from_frames(1000, 100, 142, 43_500);
    assert_eq!(optical.camera_fps, 1000);
    assert_eq!(optical.optical_latency_micros, 42_000);
    assert_eq!(optical.shutter_interval_uncertainty_micros, 1000);
    // Software measured 43.5 ms, optical measured 42.0 ms -> difference is +1.5 ms (1500 µs)
    assert_eq!(optical.software_difference_micros, 1500);
}

#[test]
fn test_instrumented_visual_target_state_machine() {
    let mut target = InstrumentedVisualTarget::default();
    assert_eq!(target.state_counter, 0);
    assert_eq!(target.visual_marker_byte, 0x00);

    // First input commit
    target.commit_input(1, 10_000_000);
    assert_eq!(target.state_counter, 1);
    assert_eq!(target.last_input_sequence, 1);
    assert_eq!(target.visual_marker_byte, 0xFF);
    assert_eq!(target.visual_commit_timestamp_ns, 10_000_000);
    assert!(target.verify_surface_marker(0xFF));
    assert!(target.verify_surface_marker(0xF0)); // Within compression tolerance
    assert!(!target.verify_surface_marker(0x05));

    // Second input commit toggles back
    target.commit_input(2, 20_000_000);
    assert_eq!(target.state_counter, 2);
    assert_eq!(target.last_input_sequence, 2);
    assert_eq!(target.visual_marker_byte, 0x00);
    assert!(target.verify_surface_marker(0x00));
}

#[test]
fn test_full_latency_benchmark_e2e_with_artifacts() {
    let test_dir = PathBuf::from("artifacts/test_latency_e2e");
    let harness = E2eHarness::new(HarnessConfig {
        artifacts_dir: test_dir.clone(),
        max_queue_bytes: 65536,
    });

    let (harness_report, latency_report) = harness.run_latency_benchmark(42, 10).unwrap();

    assert!(harness_report.is_success());
    assert!(latency_report.calibration_verified);
    assert_eq!(latency_report.stage_breakdown.len(), 9);
    assert_eq!(latency_report.total_latency.sample_count, 10);

    // Verify persisted report file exists and is valid JSON
    let report_file = harness_report.artifact_dir.join("latency_report.json");
    assert!(
        report_file.exists(),
        "latency_report.json must be written to artifact dir"
    );

    let report_content = fs::read_to_string(&report_file).unwrap();
    assert!(report_content.contains("\"calibration_verified\": true"));
    assert!(report_content.contains("\"input_transit\""));
    assert!(report_content.contains("\"display_wait\""));
    assert!(report_content.contains("\"is_instrumented_workload\": true"));
}
