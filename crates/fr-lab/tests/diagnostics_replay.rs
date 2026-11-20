//! End-to-end tests for Plan §§3.4, 18.4 & bead `fr-p4-diagnostics-replay-996`:
//! - Bounded diagnostic ring buffer (count AND bytes)
//! - Privacy sanitization with planted sensitive strings (hostnames, IPs, paths,
//!   emails, PEM headers, quoted window titles, nonces/tokens)
//! - Export rate limiting via `ProtocolLimits::max_diagnostic_exports_per_min`
//! - Deterministic "why does this feel slow?" causal attribution (and honest
//!   `InsufficientEvidence` / `Healthy` verdicts)
//! - Bit-exact replay of sanitized JSON bundles through production
//!   `fr_media::quality::QualityController`, plus planted negative divergence.
#![forbid(unsafe_code)]

use fr_client::diagnostics::{
    CodecPathKind, DiagnosticBundle, DiagnosticError, DiagnosticEventKind, DiagnosticLimits,
    DiagnosticRing, NetworkPathKind, NoteCategory, RecoveryEventKind, SlownessCause,
    SlownessVerdict, SurfaceCopyMode, TransportKind, sanitize_diagnostic_text,
};
use fr_core::limits::ProtocolLimits;
use fr_media::quality::{
    DecoderMetrics, EncoderMetrics, InputSnapshot, NetworkMetrics, PresentationMetrics,
    QualityController, QualityPolicy,
};
use fr_wire::receiver_metrics::Load as WireDecoderLoad;

fn healthy_snapshot(now_us: u64) -> InputSnapshot {
    InputSnapshot {
        now_us,
        network: NetworkMetrics {
            rtt_us: Some(12_000),
            loss_basis_points: Some(0),
            send_queue_bytes: 1_000,
            send_queue_capacity: 100_000,
            sustainable_delivery_estimate_bps: Some(25_000_000),
        },
        encoder: EncoderMetrics {
            encode_latency_us: Some(6_000),
            in_flight_surfaces: 1,
        },
        decoder: DecoderMetrics {
            load: Some(WireDecoderLoad {
                retained_bytes: 4_096,
                retained_pictures: 1,
                decoding: true,
                work_us: Some(5_000),
            }),
        },
        presentation: PresentationMetrics {
            is_visible: true,
            presentation_latency_us: Some(8_000),
        },
        application_limited: false,
        route_changed: false,
        reconnected: false,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn sanitized_bundle_scrubs_all_planted_secrets_and_replays_controller_bit_exact() {
    let policy = QualityPolicy::desktop_default();
    let mut controller = QualityController::new(policy).unwrap();
    let mut ring = DiagnosticRing::new(policy, DiagnosticLimits::default()).unwrap();

    ring.record(
        0,
        DiagnosticEventKind::SessionStarted {
            protocol_version: 0,
            transport: TransportKind::NativeQuic,
            path: NetworkPathKind::Direct,
            codec_backend: CodecPathKind::HardwareVideoToolbox,
            capture_width: 2560,
            capture_height: 1440,
            target_fps: 60,
        },
    )
    .unwrap();

    // Plant sensitive strings across every redaction class.
    let planted_secrets = [
        "mac-studio.tail9f81a.ts.net",
        "100.74.254.33",
        "fd7a:115c:a1e0::1",
        "/Users/alice/Documents/tax-return-2026.pdf",
        "C:\\Users\\alice\\Secrets\\keys.txt",
        "alice@example.com",
        "My Secret Banking Window",
        "supersecret99887766554433221100",
        "-----BEGIN PRIVATE KEY-----",
    ];
    ring.record(
        10_000,
        DiagnosticEventKind::SanitizedNote {
            category: NoteCategory::Transport,
            message: "peer mac-studio.tail9f81a.ts.net at 100.74.254.33:8443 \
                      [fd7a:115c:a1e0::1]:8443 user=alice@example.com \
                      token=supersecret99887766554433221100"
                .to_string(),
        },
    )
    .unwrap();
    ring.record(
        20_000,
        DiagnosticEventKind::SanitizedNote {
            category: NoteCategory::Lifecycle,
            message: "failed path /Users/alice/Documents/tax-return-2026.pdf and \
                      C:\\Users\\alice\\Secrets\\keys.txt window=\"My Secret Banking Window\""
                .to_string(),
        },
    )
    .unwrap();
    ring.record(
        30_000,
        DiagnosticEventKind::SanitizedNote {
            category: NoteCategory::Authority,
            message: "-----BEGIN PRIVATE KEY----- MIIEvgIBADANBg...".to_string(),
        },
    )
    .unwrap();

    // Drive a realistic multi-phase session through the real production QualityController:
    // 1. Healthy baseline
    // 2. Sustained network congestion (hotel link backoff)
    // 3. Idle application-limited hold
    // 4. Sustained headroom recovery probe
    // 5. Route change conservative reset
    let mut t = 100_000_u64;
    for _ in 0..4 {
        let dec = controller.update(healthy_snapshot(t)).unwrap();
        ring.record_controller_decision(&dec).unwrap();
        t += 50_000;
    }

    // Phase 2: 600 ms of network congestion (high RTT + loss + full send queue).
    for _ in 0..12 {
        let mut s = healthy_snapshot(t);
        s.network.rtt_us = Some(180_000);
        s.network.loss_basis_points = Some(350);
        s.network.send_queue_bytes = 90_000;
        let dec = controller.update(s).unwrap();
        ring.record_controller_decision(&dec).unwrap();
        t += 50_000;
    }
    let after_backoff_bps = controller.current_point().target_bitrate_bps;
    assert!(
        after_backoff_bps < policy.initial_bitrate_bps,
        "network congestion must reduce bitrate"
    );

    // Phase 3: Idle static desktop -> application-limited hold (never probes upward).
    for _ in 0..6 {
        let mut s = healthy_snapshot(t);
        s.application_limited = true;
        let dec = controller.update(s).unwrap();
        ring.record_controller_decision(&dec).unwrap();
        t += 50_000;
    }
    assert_eq!(
        controller.current_point().target_bitrate_bps,
        after_backoff_bps
    );

    // Phase 4: 2.5 seconds of clean headroom -> upward probe fires.
    for _ in 0..50 {
        let dec = controller.update(healthy_snapshot(t)).unwrap();
        ring.record_controller_decision(&dec).unwrap();
        t += 50_000;
    }
    assert!(
        controller.current_point().target_bitrate_bps > after_backoff_bps,
        "sustained headroom must trigger upward probe"
    );

    // Phase 5: Route switch -> conservative reset.
    let mut route_switch = healthy_snapshot(t);
    route_switch.route_changed = true;
    let dec = controller.update(route_switch).unwrap();
    ring.record_controller_decision(&dec).unwrap();

    // Export sanitized JSON bundle.
    let bundle = ring.export_bundle(t, &ProtocolLimits::ABSOLUTE).unwrap();
    let json = bundle.to_sanitized_json().unwrap();

    // Verify ZERO planted secrets survived anywhere in the exported JSON.
    for secret in planted_secrets {
        assert!(
            !json.contains(secret),
            "planted secret leaked into exported diagnostic bundle: {secret}"
        );
    }

    // Verify causal attribution correctly identifies NetworkQueueGrowth as primary cause.
    assert_eq!(bundle.slowness.verdict, SlownessVerdict::Degraded);
    assert_eq!(
        bundle.slowness.primary_cause,
        Some(SlownessCause::NetworkQueueGrowth)
    );

    // Re-import JSON and replay deterministically through a fresh production QualityController.
    let imported = DiagnosticBundle::from_json(&json).unwrap();
    let replay = imported.replay_quality_controller().unwrap();
    assert!(
        replay.is_exact_match(),
        "sanitized trace must replay bit-exact: {replay:?}"
    );
    assert_eq!(replay.steps_replayed, 4 + 12 + 6 + 50 + 1);

    // Planted negative: mutate one recorded snapshot's RTT/loss/queue during the congestion
    // window so the replayed controller does NOT back off -> replay MUST report a mismatch.
    let mut tampered = imported;
    for ev in &mut tampered.events {
        if let DiagnosticEventKind::ControllerStep { snapshot, .. } = &mut ev.kind
            && snapshot.rtt_us == Some(180_000)
        {
            snapshot.rtt_us = Some(10_000);
            snapshot.loss_basis_points = Some(0);
            snapshot.send_queue_bytes = 0;
        }
    }
    let bad_replay = tampered.replay_quality_controller().unwrap();
    assert!(
        !bad_replay.is_exact_match(),
        "planted snapshot tampering MUST break deterministic replay match"
    );
    assert!(bad_replay.first_mismatch.is_some());
}

#[test]
fn slowness_attribution_distinguishes_every_plan_bottleneck_and_never_fabricates() {
    let policy = QualityPolicy::desktop_default();

    // 1. Empty ring -> InsufficientEvidence (never fabricates Healthy or a cause).
    let empty = DiagnosticRing::new(policy, DiagnosticLimits::default()).unwrap();
    let report = empty.diagnose_slowness();
    assert_eq!(report.verdict, SlownessVerdict::InsufficientEvidence);
    assert_eq!(report.primary_cause, None);

    // 2. Relayed DERP path + GPU-to-CPU readback copies + thermal drift + recovery storm.
    let mut ring = DiagnosticRing::new(policy, DiagnosticLimits::default()).unwrap();
    ring.record(
        1_000,
        DiagnosticEventKind::PathChanged {
            from: NetworkPathKind::Direct,
            to: NetworkPathKind::RelayedDerp,
            rtt_us: Some(95_000),
        },
    )
    .unwrap();
    // 6 samples with CPU readback copies AND thermal encode drift (8ms -> 20ms).
    for (i, enc_us) in [8_000, 8_500, 9_000, 18_000, 20_000, 22_000]
        .into_iter()
        .enumerate()
    {
        ring.record(
            10_000 + (i as u64) * 20_000,
            DiagnosticEventKind::StageTiming {
                capture_us: Some(3_000),
                gpu_copy_us: Some(7_500),
                encode_us: Some(enc_us),
                network_rtt_us: Some(95_000),
                decode_us: Some(4_000),
                present_us: Some(6_000),
                source_age_us: Some(180_000),
                copy_mode: SurfaceCopyMode::CpuReadback,
            },
        )
        .unwrap();
    }
    for f in 1..=3_u64 {
        ring.record(
            200_000 + f * 10_000,
            DiagnosticEventKind::Recovery {
                kind: RecoveryEventKind::ReliableIdrRecovery,
                frame_id: f * 100,
                recovery_bytes: 180_000,
            },
        )
        .unwrap();
    }

    let report = ring.diagnose_slowness();
    assert_eq!(report.verdict, SlownessVerdict::Degraded);
    let causes: Vec<SlownessCause> = report.causes.iter().map(|c| c.cause).collect();
    assert!(causes.contains(&SlownessCause::RelayedTailnetPath));
    assert!(causes.contains(&SlownessCause::GpuToCpuCopies));
    assert!(causes.contains(&SlownessCause::OverloadedEncoder));
    assert!(causes.contains(&SlownessCause::ThermalThrottling));
    assert!(causes.contains(&SlownessCause::CaptureFreshnessStale));
    assert!(causes.contains(&SlownessCause::RecoveryStorm));
}

#[test]
fn ring_bounds_count_bytes_and_export_rate_limit() {
    let policy = QualityPolicy::desktop_default();
    let limits = DiagnosticLimits {
        max_events: 8,
        max_bytes: 4_096,
    };
    let mut ring = DiagnosticRing::new(policy, limits).unwrap();
    for i in 0..25_u64 {
        ring.record(
            i * 1_000,
            DiagnosticEventKind::SanitizedNote {
                category: NoteCategory::Media,
                message: format!("frame interval steady step {i}"),
            },
        )
        .unwrap();
        assert!(ring.len() <= limits.max_events);
        assert!(ring.retained_bytes() <= limits.max_bytes);
    }
    assert!(ring.evicted_events() >= 17);

    // Rate limit exports using ProtocolLimits::max_diagnostic_exports_per_min.
    let proto = ProtocolLimits::ABSOLUTE;
    let max_exports = usize::try_from(proto.max_diagnostic_exports_per_min()).unwrap();
    for i in 0..max_exports {
        ring.export_bundle(1_000_000 + (i as u64) * 1_000, &proto)
            .unwrap();
    }
    assert_eq!(
        ring.export_bundle(1_000_000 + (max_exports as u64) * 1_000, &proto),
        Err(DiagnosticError::ExportRateLimited)
    );
    // After 60 s window elapses, export is permitted again.
    assert!(ring.export_bundle(62_000_000, &proto).is_ok());

    // Direct sanitizer check on long hex fingerprint.
    let cleaned = sanitize_diagnostic_text("cert 0123456789abcdef0123456789abcdef");
    assert!(!cleaned.contains("0123456789abcdef"));
    assert!(cleaned.contains("[redacted:token]"));
}
