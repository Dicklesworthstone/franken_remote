use fr_media::quality::{
    BandwidthBudget, DecisionReason, EncoderMetrics, InputSnapshot, NetworkMetrics,
    PresentationMetrics, QualityController, QualityError, QualityPolicy, WorkstationProfile,
};
use fr_wire::receiver_metrics::Load;

fn default_controller() -> QualityController {
    QualityController::new(QualityPolicy::desktop_default()).unwrap()
}

fn healthy_snapshot(now_us: u64) -> InputSnapshot {
    InputSnapshot {
        now_us,
        network: NetworkMetrics {
            rtt_us: Some(25_000), // 25ms RTT
            loss_basis_points: Some(0),
            send_queue_bytes: 10_000,
            send_queue_capacity: 100_000,
            sustainable_delivery_estimate_bps: Some(15_000_000),
        },
        encoder: EncoderMetrics {
            encode_latency_us: Some(5_000),
            in_flight_surfaces: 1,
        },
        decoder: fr_media::quality::DecoderMetrics {
            load: Some(Load {
                retained_bytes: 50_000,
                retained_pictures: 1,
                decoding: false,
                work_us: Some(6_000),
            }),
        },
        presentation: PresentationMetrics {
            is_visible: true,
            presentation_latency_us: Some(10_000),
        },
        application_limited: false,
        route_changed: false,
        reconnected: false,
    }
}

#[test]
fn policy_validation_rejects_invalid_configurations() {
    let mut bad_policy = QualityPolicy::desktop_default();
    bad_policy.min_bitrate_bps = 0;
    assert_eq!(bad_policy.validate(), Err(QualityError::InvalidPolicy));

    let mut bad_policy = QualityPolicy::desktop_default();
    bad_policy.initial_bitrate_bps = bad_policy.min_bitrate_bps - 1;
    assert_eq!(bad_policy.validate(), Err(QualityError::InvalidPolicy));

    let mut bad_policy = QualityPolicy::desktop_default();
    bad_policy.initial_bitrate_bps = bad_policy.max_bitrate_bps + 1;
    assert_eq!(bad_policy.validate(), Err(QualityError::InvalidPolicy));

    let mut bad_policy = QualityPolicy::desktop_default();
    bad_policy.min_frame_interval_us = bad_policy.max_frame_interval_us + 1;
    assert_eq!(bad_policy.validate(), Err(QualityError::InvalidPolicy));
}

#[test]
fn bandwidth_budget_partitions_accurately_and_sums_consistently() {
    for total in [1_000_000, 5_000_000, 15_000_000, 35_000_000] {
        let budget = BandwidthBudget::from_total(total);
        assert!(budget.video_bitrate_bps > 0);
        assert!(budget.keyframe_allowance_bps > 0);
        assert!(budget.retransmission_allowance_bps > 0);
        assert!(budget.audio_allowance_bps > 0);
        assert!(budget.auxiliary_allowance_bps > 0);
        assert_eq!(budget.total_bps(), total);
    }
}

#[test]
fn persistent_network_pressure_triggers_bounded_backoff_after_dwell() {
    let mut c = default_controller();
    let initial_bitrate = c.current_point().target_bitrate_bps;

    // First sample with packet loss: dwell starts, holds
    let mut s1 = healthy_snapshot(100_000);
    s1.network.loss_basis_points = Some(300); // 3% loss
    let d1 = c.update(s1).unwrap();
    assert_eq!(d1.reason, DecisionReason::Hold);
    assert_eq!(d1.current_point.target_bitrate_bps, initial_bitrate);

    // After 100ms: still under 150ms dwell threshold, should still hold
    let mut s2 = healthy_snapshot(200_000);
    s2.network.loss_basis_points = Some(300);
    let d2 = c.update(s2).unwrap();
    assert_eq!(d2.reason, DecisionReason::Hold);
    assert_eq!(d2.current_point.target_bitrate_bps, initial_bitrate);

    // After 160ms: exceeds 150ms dwell threshold, should trigger NetworkBackoff
    let mut s3 = healthy_snapshot(260_000);
    s3.network.loss_basis_points = Some(300);
    let d3 = c.update(s3).unwrap();
    assert!(matches!(
        d3.reason,
        DecisionReason::NetworkBackoff {
            packet_loss: true,
            ..
        }
    ));
    // Bitrate should be reduced by 25%
    let expected = initial_bitrate * 3 / 4;
    assert_eq!(d3.current_point.target_bitrate_bps, expected);
    assert!(d3.current_point.generation > d1.current_point.generation);
}

#[test]
fn brief_pressure_spikes_under_dwell_threshold_do_not_reduce_operating_point() {
    let mut c = default_controller();
    let initial_bitrate = c.current_point().target_bitrate_bps;

    // 50ms spike
    let mut s1 = healthy_snapshot(100_000);
    s1.network.loss_basis_points = Some(400);
    assert_eq!(c.update(s1).unwrap().reason, DecisionReason::Hold);

    // Cleared before 150ms dwell
    let s2 = healthy_snapshot(150_000);
    assert_eq!(c.update(s2).unwrap().reason, DecisionReason::Hold);

    let s3 = healthy_snapshot(300_000);
    assert_eq!(c.update(s3).unwrap().reason, DecisionReason::Hold);
    assert_eq!(c.current_point().target_bitrate_bps, initial_bitrate);
}

#[test]
fn minimum_reduce_dwell_prevents_back_to_back_reductions() {
    let mut c = default_controller();

    // Trigger initial reduction
    let mut s1 = healthy_snapshot(100_000);
    s1.network.send_queue_bytes = 80_000;
    s1.network.send_queue_capacity = 100_000; // 80% full
    c.update(s1).unwrap();

    let mut s2 = healthy_snapshot(260_000);
    s2.network.send_queue_bytes = 80_000;
    s2.network.send_queue_capacity = 100_000;
    let d2 = c.update(s2).unwrap();
    assert!(matches!(d2.reason, DecisionReason::NetworkBackoff { .. }));

    let first_reduced = d2.current_point.target_bitrate_bps;

    // Immediately after (100ms later, less than 250ms reduce dwell), even with continuous pressure:
    let mut s3 = healthy_snapshot(360_000);
    s3.network.send_queue_bytes = 80_000;
    s3.network.send_queue_capacity = 100_000;
    let d3 = c.update(s3).unwrap();
    assert_eq!(d3.reason, DecisionReason::Hold);
    assert_eq!(d3.current_point.target_bitrate_bps, first_reduced);

    // After 250ms reduce dwell (at 520_000us): second reduction permitted
    let mut s4 = healthy_snapshot(520_000);
    s4.network.send_queue_bytes = 80_000;
    s4.network.send_queue_capacity = 100_000;
    let d4 = c.update(s4).unwrap();
    assert!(matches!(d4.reason, DecisionReason::NetworkBackoff { .. }));
    assert_eq!(d4.current_point.target_bitrate_bps, first_reduced * 3 / 4);
}

#[test]
fn distinct_pressure_sources_are_accurately_attributed() {
    // 1. Encoder throttle
    {
        let mut c = default_controller();
        let mut s1 = healthy_snapshot(100_000);
        s1.encoder.in_flight_surfaces = 3; // queue backlog
        let _ = c.update(s1);

        let mut s2 = healthy_snapshot(260_000);
        s2.encoder.in_flight_surfaces = 3;
        let d = c.update(s2).unwrap();
        assert!(matches!(
            d.reason,
            DecisionReason::EncoderThrottle {
                queue_backlog: true,
                ..
            }
        ));
    }

    // 2. Decoder throttle
    {
        let mut c = default_controller();
        let mut s1 = healthy_snapshot(100_000);
        s1.decoder.load = Some(Load {
            retained_bytes: 80_000,
            retained_pictures: 3, // backlog > 1
            decoding: true,
            work_us: Some(25_000),
        });
        let _ = c.update(s1);

        let mut s2 = healthy_snapshot(260_000);
        s2.decoder.load = Some(Load {
            retained_bytes: 80_000,
            retained_pictures: 3,
            decoding: true,
            work_us: Some(25_000),
        });
        let d = c.update(s2).unwrap();
        assert!(matches!(
            d.reason,
            DecisionReason::DecoderThrottle {
                picture_backlog: true,
                ..
            }
        ));
    }

    // 3. Presentation throttle
    {
        let mut c = default_controller();
        let mut s1 = healthy_snapshot(100_000);
        s1.presentation.is_visible = false; // window hidden / minimized
        let _ = c.update(s1);

        let mut s2 = healthy_snapshot(260_000);
        s2.presentation.is_visible = false;
        let d = c.update(s2).unwrap();
        assert!(matches!(
            d.reason,
            DecisionReason::PresentationThrottle { hidden: true, .. }
        ));
    }
}

#[test]
fn text_profile_reduces_frame_rate_before_reducing_bitrate() {
    let mut policy = QualityPolicy::desktop_default();
    policy.default_profile = WorkstationProfile::Text;
    let mut c = QualityController::new(policy).unwrap();

    let initial_bitrate = c.current_point().target_bitrate_bps;
    let initial_interval = c.current_point().frame_interval_us;

    // Apply network pressure for > 150ms dwell
    let mut s1 = healthy_snapshot(100_000);
    s1.network.rtt_us = Some(150_000); // RTT inflation
    let _ = c.update(s1);

    let mut s2 = healthy_snapshot(260_000);
    s2.network.rtt_us = Some(150_000);
    let d = c.update(s2).unwrap();

    assert!(matches!(d.reason, DecisionReason::NetworkBackoff { .. }));
    // In Text profile, frame rate stepped down (interval increased) to preserve sharp spatial detail!
    assert!(d.current_point.frame_interval_us > initial_interval);
    assert_eq!(d.current_point.target_bitrate_bps, initial_bitrate);
}

#[test]
fn sustained_headroom_triggers_upward_probe_after_two_seconds() {
    let mut policy = QualityPolicy::desktop_default();
    policy.initial_bitrate_bps = 5_000_000;
    let mut c = QualityController::new(policy).unwrap();

    // Run healthy samples every 100ms: at i = 20, elapsed time reaches 2.0s (2_000_000us)
    for i in 0..=20 {
        let s = healthy_snapshot(i * 100_000);
        let d = c.update(s).unwrap();
        if i < 20 {
            assert_eq!(d.reason, DecisionReason::Hold);
        } else {
            assert_eq!(d.reason, DecisionReason::HeadroomProbe);
            let expected = 5_000_000 + 500_000; // +10%
            assert_eq!(d.current_point.target_bitrate_bps, expected);
        }
    }
}

#[test]
fn application_limited_idle_screen_never_probes_upward_and_never_backs_off() {
    let mut c = default_controller();
    let initial_bitrate = c.current_point().target_bitrate_bps;

    // Feed healthy but application-limited (screen stationary/idle) samples for 5 seconds
    for i in 0..50 {
        let mut s = healthy_snapshot(i * 100_000);
        s.application_limited = true;
        let d = c.update(s).unwrap();
        assert_eq!(d.reason, DecisionReason::ApplicationLimitedHold);
        assert_eq!(d.current_point.target_bitrate_bps, initial_bitrate);
    }
}

#[test]
fn route_change_and_reconnect_cause_conservative_resets() {
    let mut c = default_controller();

    // Probe upward first
    for i in 0..22 {
        c.update(healthy_snapshot(i * 100_000)).unwrap();
    }
    let elevated_bitrate = c.current_point().target_bitrate_bps;

    // Route change occurs (e.g. WiFi to cellular)
    let mut s_route = healthy_snapshot(2_500_000);
    s_route.route_changed = true;
    let d_route = c.update(s_route).unwrap();

    assert!(matches!(
        d_route.reason,
        DecisionReason::ConservativeReset {
            route_changed: true,
            ..
        }
    ));
    assert!(d_route.current_point.target_bitrate_bps < elevated_bitrate);

    // Reconnect occurs
    let mut s_reconn = healthy_snapshot(2_600_000);
    s_reconn.reconnected = true;
    let d_reconn = c.update(s_reconn).unwrap();

    assert!(matches!(
        d_reconn.reason,
        DecisionReason::ConservativeReset {
            reconnected: true,
            ..
        }
    ));
}

#[test]
fn clock_regression_is_strictly_rejected() {
    let mut c = default_controller();
    let _ = c.update(healthy_snapshot(500_000)).unwrap();

    let mut regressed = healthy_snapshot(400_000);
    regressed.now_us = 400_000;
    assert_eq!(c.update(regressed), Err(QualityError::ClockRegression));
}

#[test]
fn pure_function_replayability_is_100_percent_deterministic() {
    // Generate a diverse sequence of 40 snapshots
    let mut snapshots = Vec::new();
    for i in 0..40 {
        let now_us = i * 100_000;
        let mut s = healthy_snapshot(now_us);
        if (5..8).contains(&i) {
            // Transient loss spike
            s.network.loss_basis_points = Some(350);
        } else if (15..20).contains(&i) {
            // Sustained idle
            s.application_limited = true;
        } else if i == 25 {
            // Route change
            s.route_changed = true;
        } else if (30..35).contains(&i) {
            // Encoder queue buildup
            s.encoder.in_flight_surfaces = 2;
        }
        snapshots.push(s);
    }

    // Run through Run 1
    let mut controller1 = default_controller();
    let mut decisions1 = Vec::new();
    for &s in &snapshots {
        decisions1.push(controller1.update(s).unwrap());
    }

    // Run through Run 2 (fresh instance, same policy)
    let mut controller2 = default_controller();
    let mut decisions2 = Vec::new();
    for &s in &snapshots {
        decisions2.push(controller2.update(s).unwrap());
    }

    // Byte-for-byte exact equality of all decision records!
    assert_eq!(decisions1.len(), decisions2.len());
    for (d1, d2) in decisions1.iter().zip(decisions2.iter()) {
        assert_eq!(d1.reason, d2.reason);
        assert_eq!(d1.current_point, d2.current_point);
        assert_eq!(d1.headroom_us, d2.headroom_us);
        assert_eq!(d1.pressure_us, d2.pressure_us);
        assert_eq!(d1.snapshot, d2.snapshot);
    }
}
