//! Tests for the real instrumented input-to-photon latency harness
//! (`fr-p1-latency-harness-x84`, Plan §21.3).
#![forbid(unsafe_code)]

use fr_core::ids::HostBootId;
use fr_lab::latency::{
    ClockBracket, ClockMethod, LatencyError, LatencyHarness, MeasurementScope, MonotonicProbeClock,
    NetworkMode, RawLatencySample, WarmupState,
};
use fr_media::freshness::{ClockCorrelation, ClockPolicy, ClockSample};
use std::{thread, time::Duration};

fn valid_scope() -> MeasurementScope {
    MeasurementScope {
        host_os: "linux-x86_64-ubuntu-24.04".into(),
        client_os: "macos-aarch64".into(),
        process_family: "frd+fr-media-worker+fr-input-agent+fr".into(),
        capture_path: "xshm-x11".into(),
        encode_path: "hevc-nvenc".into(),
        decode_path: "videotoolbox-metal".into(),
        network_mode: NetworkMode::DirectTailnet,
        warmup: WarmupState::WarmMediaPath,
        clock_method: ClockMethod::CorrelatedClockExchange,
        width: 1920,
        height: 1080,
        refresh_hz: 60,
        tailscale_cpu_percent_tenths: Some(14),
    }
}

#[test]
fn real_monotonic_probe_reflects_actual_injected_stage_delay() {
    let clock = MonotonicProbeClock::start();
    let boot = HostBootId::from_raw(7);
    let mut scope = valid_scope();
    scope.clock_method = ClockMethod::SharedMonotonicClock;
    scope.network_mode = NetworkMode::LoopbackNamespace;

    let mut baseline = LatencyHarness::new(scope.clone(), None).unwrap();
    let mut delayed = LatencyHarness::new(scope, None).unwrap();

    // Measure 5 fast probes vs 5 probes with a real 20 ms encode wait.
    for i in 1..=5_u64 {
        let t0 = clock.now_us();
        let t1 = clock.now_us();
        let t2 = clock.now_us();
        let t3 = clock.now_us();
        let t4 = clock.now_us();
        let t5 = clock.now_us();
        let t6 = clock.now_us();
        let t7 = clock.now_us();
        let t8 = clock.now_us();
        let t9 = clock.now_us();
        let t10 = clock.now_us();
        let t11 = clock.now_us();
        baseline
            .record_sample(RawLatencySample {
                sample_id: i,
                host_boot: boot,
                client_input_captured_us: t0,
                client_input_submitted_us: t1,
                host_input_received_us: t2,
                host_target_changed_us: t3,
                host_frame_captured_us: t4,
                host_frame_converted_us: t5,
                host_frame_encoded_us: t6,
                host_packet_sent_us: t7,
                client_first_packet_received_us: t8,
                client_frame_reassembled_us: t9,
                client_frame_decoded_us: t10,
                client_frame_presented_us: t11,
            })
            .unwrap();

        let d0 = clock.now_us();
        let d1 = clock.now_us();
        let d2 = clock.now_us();
        let d3 = clock.now_us();
        let d4 = clock.now_us();
        let d5 = clock.now_us();
        thread::sleep(Duration::from_millis(20));
        let d6 = clock.now_us();
        let d7 = clock.now_us();
        let d8 = clock.now_us();
        let d9 = clock.now_us();
        let d10 = clock.now_us();
        let d11 = clock.now_us();
        delayed
            .record_sample(RawLatencySample {
                sample_id: i,
                host_boot: boot,
                client_input_captured_us: d0,
                client_input_submitted_us: d1,
                host_input_received_us: d2,
                host_target_changed_us: d3,
                host_frame_captured_us: d4,
                host_frame_converted_us: d5,
                host_frame_encoded_us: d6,
                host_packet_sent_us: d7,
                client_first_packet_received_us: d8,
                client_frame_reassembled_us: d9,
                client_frame_decoded_us: d10,
                client_frame_presented_us: d11,
            })
            .unwrap();
    }

    let base_report = baseline.finish().unwrap();
    let delay_report = delayed.finish().unwrap();
    assert_eq!(base_report.sample_count, 5);
    assert_eq!(delay_report.sample_count, 5);
    // Real injected 20 ms encode wait MUST show up in BOTH encode and input-to-photon p50/p95.
    assert!(
        delay_report.encode.p50_us >= base_report.encode.p50_us + 18_000,
        "expected >=18 ms real encode delta: base={}, delayed={}",
        base_report.encode.p50_us,
        delay_report.encode.p50_us
    );
    assert!(
        delay_report.input_to_photon.p95_us >= base_report.input_to_photon.p95_us + 18_000,
        "expected >=18 ms real input-to-photon delta"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn correlated_clock_exchange_bounds_cross_host_transit_and_rejects_bad_inputs() {
    let boot = HostBootId::from_raw(42);
    // Construct a real ClockCorrelation from fr-media with unrelated host/client clock origins.
    let sample = ClockSample {
        host_boot: boot,
        client_sent_us: 1_000_000,
        host_sample_us: 900_000_000, // Completely unrelated origin (+899 s offset)
        client_received_us: 1_006_000, // 6 ms exchange RTT
    };
    let corr = ClockCorrelation::new(sample, ClockPolicy::default()).unwrap();
    let bracket = ClockBracket {
        host_boot: corr.host_boot(),
        client_sent_us: sample.client_sent_us,
        host_sample_us: sample.host_sample_us,
        client_received_us: corr.received_at_us(),
        drift_ppm: ClockPolicy::default().drift_ppm,
    };

    let mut harness = LatencyHarness::new(valid_scope(), Some(bracket)).unwrap();
    for i in 1..=20_u64 {
        let base_c = 1_100_000 + i * 20_000;
        let base_h = 900_100_000 + i * 20_000;
        let encode_extra = if i == 20 { 15_000 } else { 0 };
        harness
            .record_sample(RawLatencySample {
                sample_id: i,
                host_boot: boot,
                client_input_captured_us: base_c,
                client_input_submitted_us: base_c + 500,
                host_input_received_us: base_h + 500,
                host_target_changed_us: base_h + 2_500,
                host_frame_captured_us: base_h + 6_500,
                host_frame_converted_us: base_h + 7_500,
                host_frame_encoded_us: base_h + 13_500 + encode_extra,
                host_packet_sent_us: base_h + 14_000 + encode_extra,
                client_first_packet_received_us: base_c + 20_000 + encode_extra,
                client_frame_reassembled_us: base_c + 21_000 + encode_extra,
                client_frame_decoded_us: base_c + 26_000 + encode_extra,
                client_frame_presented_us: base_c + 34_000 + encode_extra,
            })
            .unwrap();
    }
    harness.record_dropped_probe();
    harness.record_stale_view_interval();

    let report = harness.finish().unwrap();
    assert_eq!(report.sample_count, 20);
    assert_eq!(report.dropped_probes, 1);
    assert_eq!(report.stale_view_intervals, 1);
    assert_eq!(report.input_to_photon.p50_us, 34_000);
    assert_eq!(report.input_to_photon.p95_us, 34_000);
    assert_eq!(report.input_to_photon.p99_us, 49_000);
    assert_eq!(report.encode.p50_us, 6_000);
    assert_eq!(report.encode.max_us, 21_000);
    assert!(report.input_transit.is_some());
    assert!(report.return_transit.is_some());
    assert!(
        report.max_cross_host_uncertainty_us.unwrap() >= 6_000,
        "uncertainty must cover full 6 ms clock-exchange interval"
    );

    // Planted negatives:
    // 1. Missing scope refused.
    let mut bad_scope = valid_scope();
    bad_scope.encode_path = "   ".into();
    assert_eq!(
        LatencyHarness::new(bad_scope, None).unwrap_err(),
        LatencyError::IncompleteScope
    );

    // 2. Inverted / fabricated timestamp refused.
    let inverted = RawLatencySample {
        sample_id: 99,
        host_boot: boot,
        client_input_captured_us: 1000,
        client_input_submitted_us: 900, // regressed!
        host_input_received_us: 1000,
        host_target_changed_us: 1000,
        host_frame_captured_us: 1000,
        host_frame_converted_us: 1000,
        host_frame_encoded_us: 1000,
        host_packet_sent_us: 1000,
        client_first_packet_received_us: 1000,
        client_frame_reassembled_us: 1000,
        client_frame_decoded_us: 1000,
        client_frame_presented_us: 1000,
    };
    assert_eq!(
        harness.record_sample(inverted),
        Err(LatencyError::ClockRegression)
    );

    // 3. Host boot mismatch against clock bracket refused.
    let wrong_boot = RawLatencySample {
        sample_id: 100,
        host_boot: HostBootId::from_raw(999),
        client_input_captured_us: 1000,
        client_input_submitted_us: 1000,
        host_input_received_us: 1000,
        host_target_changed_us: 1000,
        host_frame_captured_us: 1000,
        host_frame_converted_us: 1000,
        host_frame_encoded_us: 1000,
        host_packet_sent_us: 1000,
        client_first_packet_received_us: 1000,
        client_frame_reassembled_us: 1000,
        client_frame_decoded_us: 1000,
        client_frame_presented_us: 1000,
    };
    assert_eq!(
        harness.record_sample(wrong_boot),
        Err(LatencyError::HostBootMismatch)
    );
}
