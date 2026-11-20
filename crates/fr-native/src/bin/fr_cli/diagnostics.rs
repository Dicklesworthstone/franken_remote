#![forbid(unsafe_code)]
//! Bounded, sanitized diagnostic bundle export and deterministic local
//! controller replay for `fr` (`fr-p4-diagnostics-replay-996`, Plan §§3.4, 18.4).
//!
//! Replay re-executes recorded `QualityController` decisions in software only;
//! it never claims to reproduce GPU drivers, capture hardware, or live networks.
use crate::Failure;
#[cfg(any(
    feature = "linux-desktop",
    all(target_os = "macos", feature = "macos-desktop"),
    all(test, target_os = "linux")
))]
use fr_client::diagnostics::{
    CodecPathKind, DiagnosticEventKind, DiagnosticLimits, DiagnosticRing, NetworkPathKind,
    NoteCategory, RecoveryEventKind, SurfaceCopyMode, TransportKind,
};
use fr_client::diagnostics::{
    DiagnosticBundle, DiagnosticError, MAX_IMPORT_JSON_BYTES, SlownessCause, SlownessReport,
    SlownessVerdict,
};
#[cfg(any(
    feature = "linux-desktop",
    all(target_os = "macos", feature = "macos-desktop"),
    all(test, target_os = "linux")
))]
use fr_core::limits::ProtocolLimits;
#[cfg(any(
    feature = "linux-desktop",
    all(target_os = "macos", feature = "macos-desktop"),
    all(test, target_os = "linux")
))]
use fr_media::quality::QualityPolicy;
use serde::Serialize;
#[cfg(any(
    feature = "linux-desktop",
    all(target_os = "macos", feature = "macos-desktop"),
    all(test, target_os = "linux")
))]
use std::io::Write;
use std::{
    fmt::Write as _,
    fs::File,
    io::Read,
    path::{Component, Path},
};

const REPLAY_SCOPE: &str = "controller_decisions_only_not_gpu_drivers";

#[derive(Debug, Serialize)]
struct ReplayJsonOutput<'a> {
    schema_version: u16,
    replay_scope: &'static str,
    deterministic: bool,
    steps_replayed: usize,
    matched_steps: usize,
    evicted_events: u64,
    slowness: &'a SlownessReport,
    refusal_code: Option<&'static str>,
    first_mismatch: Option<MismatchJson>,
}

#[derive(Debug, Serialize)]
struct MismatchJson {
    step_index: usize,
    expected_bitrate_bps: u64,
    replayed_bitrate_bps: u64,
    expected_interval_us: u64,
    replayed_interval_us: u64,
}

fn validate_local_path(path: &Path) -> Result<(), Failure> {
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path.components().any(|c| matches!(c, Component::ParentDir))
        || path.file_name().is_none()
    {
        return Err(Failure::new(
            "diagnostics_unsafe_path",
            "Use an absolute local regular-file path without '..'; symlinks are refused.",
            2,
        ));
    }
    Ok(())
}

fn read_bounded_regular_file(path: &Path) -> Result<String, Failure> {
    validate_local_path(path)?;
    let file = open_regular_readonly(path)?;
    let max_bytes = u64::try_from(MAX_IMPORT_JSON_BYTES).unwrap_or(u64::MAX);
    let mut raw = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut raw)
        .map_err(|_| {
            Failure::new(
                "diagnostics_io_failed",
                "Could not read the local diagnostic bundle; verify file permissions.",
                1,
            )
        })?;
    if raw.len() > MAX_IMPORT_JSON_BYTES {
        return Err(Failure::new(
            "diagnostics_bundle_too_large",
            "Diagnostic bundle exceeds the 2 MiB import limit and was refused before parsing.",
            1,
        ));
    }
    String::from_utf8(raw).map_err(|_| {
        Failure::new(
            "diagnostics_invalid_bundle",
            "Diagnostic bundle must be valid UTF-8 JSON matching schema version 1.",
            1,
        )
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_regular_readonly(path: &Path) -> Result<File, Failure> {
    use rustix::fs::{self, FileType, Mode, OFlags};
    let fd = fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .map_err(|errno| {
        if errno == rustix::io::Errno::LOOP {
            Failure::new(
                "diagnostics_unsafe_path",
                "Symlink diagnostic paths are refused; select a local regular file.",
                1,
            )
        } else {
            Failure::new(
                "diagnostics_io_failed",
                "Could not open the local diagnostic bundle; verify the file exists and is readable.",
                1,
            )
        }
    })?;
    let stat = fs::fstat(&fd).map_err(|_| {
        Failure::new(
            "diagnostics_io_failed",
            "Could not inspect the local diagnostic bundle metadata.",
            1,
        )
    })?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(Failure::new(
            "diagnostics_unsafe_path",
            "Diagnostic bundle path must be a regular file, not a directory, FIFO, or device.",
            1,
        ));
    }
    Ok(File::from(fd))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn open_regular_readonly(path: &Path) -> Result<File, Failure> {
    let meta = std::fs::symlink_metadata(path).map_err(|_| {
        Failure::new(
            "diagnostics_io_failed",
            "Could not inspect the local diagnostic bundle; verify the file exists and is readable.",
            1,
        )
    })?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(Failure::new(
            "diagnostics_unsafe_path",
            "Diagnostic bundle path must be a local regular file; symlinks are refused.",
            1,
        ));
    }
    File::open(path).map_err(|_| {
        Failure::new(
            "diagnostics_io_failed",
            "Could not open the local diagnostic bundle; verify file permissions.",
            1,
        )
    })
}

#[cfg(all(
    any(target_os = "linux", target_os = "macos"),
    any(
        feature = "linux-desktop",
        all(target_os = "macos", feature = "macos-desktop"),
        all(test, target_os = "linux")
    )
))]
fn write_private_regular_file(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    use rustix::fs::{self, FileType, Mode, OFlags};
    let fd = fs::open(
        path,
        OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|errno| {
        if errno == rustix::io::Errno::LOOP {
            Failure::new(
                "diagnostics_unsafe_path",
                "Refusing to export diagnostics through a symlink; use a private regular file path.",
                1,
            )
        } else {
            Failure::new(
                "diagnostics_io_failed",
                "Could not open the diagnostic export destination; verify parent directory permissions.",
                1,
            )
        }
    })?;
    let stat = fs::fstat(&fd).map_err(|_| {
        Failure::new(
            "diagnostics_io_failed",
            "Could not verify the diagnostic export file type.",
            1,
        )
    })?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(Failure::new(
            "diagnostics_unsafe_path",
            "Diagnostic export destination must be a regular file.",
            1,
        ));
    }
    let mut file = File::from(fd);
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| {
            Failure::new(
                "diagnostics_io_failed",
                "Could not write the sanitized diagnostic bundle to disk.",
                1,
            )
        })
}

#[cfg(all(
    not(any(target_os = "linux", target_os = "macos")),
    any(
        feature = "linux-desktop",
        all(target_os = "macos", feature = "macos-desktop"),
        all(test, target_os = "linux")
    )
))]
fn write_private_regular_file(path: &Path, bytes: &[u8]) -> Result<(), Failure> {
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err(Failure::new(
                "diagnostics_unsafe_path",
                "Refusing to export diagnostics to a symlink or non-regular file.",
                1,
            ));
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|_| {
            Failure::new(
                "diagnostics_io_failed",
                "Could not open the diagnostic export destination.",
                1,
            )
        })?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| {
            Failure::new(
                "diagnostics_io_failed",
                "Could not write the sanitized diagnostic bundle to disk.",
                1,
            )
        })
}

fn map_bundle_error(err: DiagnosticError) -> Failure {
    match err {
        DiagnosticError::BundleTooLarge => Failure::new(
            "diagnostics_bundle_too_large",
            "Diagnostic bundle exceeds the 2 MiB import limit.",
            1,
        ),
        DiagnosticError::UnsupportedSchemaVersion(_) => Failure::new(
            "diagnostics_unsupported_schema",
            "Diagnostic bundle schema version is unsupported by this version of fr.",
            1,
        ),
        DiagnosticError::InvalidPolicy => Failure::new(
            "diagnostics_invalid_policy",
            "Diagnostic bundle carries an invalid QualityPolicy and was refused.",
            1,
        ),
        DiagnosticError::ExportRateLimited => Failure::new(
            "diagnostics_rate_limited",
            "Diagnostic export rate limit reached; retry after the current window.",
            1,
        ),
        DiagnosticError::DeserializationFailed
        | DiagnosticError::SerializationFailed
        | DiagnosticError::InvalidLimits
        | DiagnosticError::EventTooLarge
        | DiagnosticError::ClockRegression
        | DiagnosticError::ReplayError(_) => Failure::new(
            "diagnostics_invalid_bundle",
            "Diagnostic bundle could not be parsed or replayed; inspect the bundle locally.",
            1,
        ),
    }
}

/// Local, offline deterministic replay and slowness-attribution inspection
/// (`fr doctor --replay-diagnostics /abs/path.json [--json]`).
pub(crate) fn replay(path: &Path, json: bool) -> Result<(String, u8), Failure> {
    let raw = read_bounded_regular_file(path)?;
    let parsed = DiagnosticBundle::from_json(&raw).map_err(map_bundle_error)?;
    // Re-sanitize and recompute slowness attribution strictly from sanitized events
    // so an untrusted bundle cannot inject a fabricated slowness verdict or raw note.
    let clean_json = parsed.to_sanitized_json().map_err(map_bundle_error)?;
    let bundle = DiagnosticBundle::from_json(&clean_json).map_err(map_bundle_error)?;

    let first = bundle
        .replay_quality_controller()
        .map_err(map_bundle_error)?;
    let second = bundle
        .replay_quality_controller()
        .map_err(map_bundle_error)?;
    let deterministic = first == second;
    let mismatch_detected = !deterministic || first.first_mismatch.is_some();
    let refusal_code = if !deterministic {
        Some("diagnostics_replay_nondeterministic")
    } else if first.first_mismatch.is_some() {
        Some("diagnostics_replay_mismatch")
    } else {
        None
    };
    let exit = u8::from(mismatch_detected);

    let rendered = if json {
        let output = ReplayJsonOutput {
            schema_version: bundle.schema_version,
            replay_scope: REPLAY_SCOPE,
            deterministic,
            steps_replayed: first.steps_replayed,
            matched_steps: first.matched_steps,
            evicted_events: bundle.evicted_events,
            slowness: &bundle.slowness,
            refusal_code,
            first_mismatch: first.first_mismatch.as_ref().map(|m| MismatchJson {
                step_index: m.step_index,
                expected_bitrate_bps: m.expected_bitrate_bps,
                replayed_bitrate_bps: m.replayed_bitrate_bps,
                expected_interval_us: m.expected_interval_us,
                replayed_interval_us: m.replayed_interval_us,
            }),
        };
        let mut s = serde_json::to_string(&output).map_err(|_| {
            Failure::new(
                "diagnostics_serialization_failed",
                "Could not format the diagnostic replay JSON report.",
                1,
            )
        })?;
        s.push('\n');
        s
    } else {
        render_text(&bundle, &first, deterministic, refusal_code)
    };
    Ok((rendered, exit))
}

fn verdict_label(verdict: SlownessVerdict) -> &'static str {
    match verdict {
        SlownessVerdict::InsufficientEvidence => "insufficient_evidence",
        SlownessVerdict::Healthy => "healthy",
        SlownessVerdict::Degraded => "degraded",
    }
}

fn cause_label(cause: SlownessCause) -> &'static str {
    match cause {
        SlownessCause::RelayedTailnetPath => "relayed_tailnet_path",
        SlownessCause::NetworkQueueGrowth => "network_queue_growth",
        SlownessCause::OverloadedEncoder => "overloaded_encoder",
        SlownessCause::GpuToCpuCopies => "gpu_to_cpu_copies",
        SlownessCause::DecoderBacklog => "decoder_backlog",
        SlownessCause::PresentationStall => "presentation_stall",
        SlownessCause::ThermalThrottling => "thermal_throttling",
        SlownessCause::CaptureFreshnessStale => "capture_freshness_stale",
        SlownessCause::RecoveryStorm => "recovery_storm",
    }
}

fn render_text(
    bundle: &DiagnosticBundle,
    report: &fr_client::diagnostics::ReplayReport,
    deterministic: bool,
    refusal_code: Option<&'static str>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Diagnostic replay (scope: {REPLAY_SCOPE}; controller decisions only, not GPU drivers or live network)."
    );
    let _ = writeln!(
        out,
        "Schema v{}; controller steps replayed: {}/{} matched; deterministic double-run: {}; evicted ring events: {}.",
        bundle.schema_version,
        report.matched_steps,
        report.steps_replayed,
        deterministic,
        bundle.evicted_events,
    );
    let _ = write!(
        out,
        "Slowness verdict: {} (stage samples: {}, controller steps: {})",
        verdict_label(bundle.slowness.verdict),
        bundle.slowness.stage_samples,
        bundle.slowness.controller_steps,
    );
    if let Some(primary) = bundle.slowness.primary_cause {
        let _ = write!(out, "; primary cause: {}", cause_label(primary));
    }
    out.push_str(".\n");
    for attributed in &bundle.slowness.causes {
        let _ = writeln!(
            out,
            "  - {} ({:?}, {} sample(s), peak_us={:?})",
            cause_label(attributed.cause),
            attributed.severity,
            attributed.evidence_samples,
            attributed.peak_metric_us,
        );
    }
    if let Some(mismatch) = &report.first_mismatch {
        let _ = writeln!(
            out,
            "Mismatch at step {}: expected {} bps / {} us, replayed {} bps / {} us.",
            mismatch.step_index,
            mismatch.expected_bitrate_bps,
            mismatch.expected_interval_us,
            mismatch.replayed_bitrate_bps,
            mismatch.replayed_interval_us,
        );
    }
    if let Some(code) = refusal_code {
        let _ = writeln!(out, "Refusal: {code}.");
    }
    out
}

/// Export a sanitized diagnostic bundle from real completed-session statistics.
/// Never fabricates metrics or healthy signals when evidence is absent.
#[cfg(any(
    feature = "linux-desktop",
    all(target_os = "macos", feature = "macos-desktop"),
    all(test, target_os = "linux")
))]
pub(crate) fn export_session(
    path: &Path,
    media: Option<&frd::session_startup::ViewerStatistics>,
    codec: CodecPathKind,
    transport: TransportKind,
    note: Option<&str>,
) -> Result<(), Failure> {
    validate_local_path(path)?;
    let mut ring = DiagnosticRing::new(
        QualityPolicy::desktop_default(),
        DiagnosticLimits::default(),
    )
    .map_err(map_bundle_error)?;
    let mut now_us = 1_000_u64;

    if let Some(stats) = media {
        if let Some((width, height)) = stats.picture {
            let _ = ring.record(
                now_us,
                DiagnosticEventKind::SessionStarted {
                    protocol_version: 0,
                    transport,
                    path: NetworkPathKind::Unknown,
                    codec_backend: codec,
                    capture_width: width,
                    capture_height: height,
                    target_fps: 0,
                },
            );
            now_us = now_us.saturating_add(1_000);
        }
        if let Some(p95_age_us) = stats.presented_age.quantile_upper_us(950) {
            let _ = ring.record(
                now_us,
                DiagnosticEventKind::StageTiming {
                    capture_us: None,
                    gpu_copy_us: None,
                    encode_us: None,
                    network_rtt_us: None,
                    decode_us: None,
                    present_us: None,
                    source_age_us: Some(p95_age_us),
                    copy_mode: SurfaceCopyMode::Unknown,
                },
            );
            now_us = now_us.saturating_add(1_000);
        }
        if stats.recovered_streams > 0 {
            let _ = ring.record(
                now_us,
                DiagnosticEventKind::Recovery {
                    kind: RecoveryEventKind::ReliableIdrRecovery,
                    frame_id: stats.decoded,
                    recovery_bytes: 0,
                },
            );
            now_us = now_us.saturating_add(1_000);
        } else if stats.repair_requests > 0 {
            let _ = ring.record(
                now_us,
                DiagnosticEventKind::Recovery {
                    kind: RecoveryEventKind::SelectiveRepair,
                    frame_id: stats.decoded,
                    recovery_bytes: 0,
                },
            );
            now_us = now_us.saturating_add(1_000);
        }
        if let Some(decision) = stats.last_host_quality_decision {
            let _ = ring.record(
                now_us,
                DiagnosticEventKind::SanitizedNote {
                    category: NoteCategory::Media,
                    message: format!(
                        "host_quality seq={} interval_us={} mode={:?} reason={:?}",
                        decision.sequence,
                        decision.capture_interval_us,
                        decision.mode,
                        decision.reason
                    ),
                },
            );
            now_us = now_us.saturating_add(1_000);
        }
    }
    if let Some(note) = note {
        let _ = ring.record(
            now_us,
            DiagnosticEventKind::SanitizedNote {
                category: NoteCategory::Lifecycle,
                message: note.to_owned(),
            },
        );
        now_us = now_us.saturating_add(1_000);
    }

    let bundle = ring
        .export_bundle(now_us, &ProtocolLimits::ABSOLUTE)
        .map_err(map_bundle_error)?;
    let mut json = bundle.to_sanitized_json().map_err(map_bundle_error)?;
    json.push('\n');
    write_private_regular_file(path, json.as_bytes())
}

// The tests export through frd's viewer statistics, which a macOS build
// without `macos-desktop` does not compile.
#[cfg(all(
    test,
    any(
        target_os = "linux",
        all(target_os = "macos", feature = "macos-desktop")
    )
))]
mod tests {
    use super::*;
    use fr_media::quality::{
        DecoderMetrics, EncoderMetrics, InputSnapshot, NetworkMetrics, PresentationMetrics,
        QualityController,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("fr-cli-diag-{label}-{}-{id}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn build_real_controller_bundle() -> DiagnosticBundle {
        let policy = QualityPolicy::desktop_default();
        let mut controller = QualityController::new(policy).unwrap();
        let mut ring = DiagnosticRing::new(policy, DiagnosticLimits::default()).unwrap();
        ring.record(
            100_000,
            DiagnosticEventKind::SessionStarted {
                protocol_version: 0,
                transport: TransportKind::NativeQuic,
                path: NetworkPathKind::RelayedDerp,
                codec_backend: CodecPathKind::HardwareVideoToolbox,
                capture_width: 2560,
                capture_height: 1440,
                target_fps: 60,
            },
        )
        .unwrap();
        // Plant sensitive strings that MUST be scrubbed on export and replay.
        ring.record(
            120_000,
            DiagnosticEventKind::SanitizedNote {
                category: NoteCategory::Transport,
                message: "peer mac-studio.tail1234.ts.net [fd7a:115c:a1e0::1]:8443 /Users/alice/secret.txt alice@example.com title=\"Payroll 2026\" token=0123456789abcdef0123456789abcdef"
                    .to_owned(),
            },
        )
        .unwrap();
        for i in 0_u64..4 {
            let now_us = 200_000 + i * 250_000;
            ring.record(
                now_us,
                DiagnosticEventKind::StageTiming {
                    capture_us: Some(2_000),
                    gpu_copy_us: Some(8_500),
                    encode_us: Some(5_000),
                    network_rtt_us: Some(75_000),
                    decode_us: Some(4_000),
                    present_us: Some(3_000),
                    source_age_us: Some(35_000),
                    copy_mode: SurfaceCopyMode::CpuReadback,
                },
            )
            .unwrap();
            let snapshot = InputSnapshot {
                now_us,
                network: NetworkMetrics {
                    rtt_us: Some(75_000),
                    loss_basis_points: Some(120),
                    send_queue_bytes: 200_000,
                    send_queue_capacity: 1_000_000,
                    sustainable_delivery_estimate_bps: Some(8_000_000),
                },
                encoder: EncoderMetrics {
                    encode_latency_us: Some(5_000),
                    in_flight_surfaces: 1,
                },
                decoder: DecoderMetrics {
                    load: Some(fr_wire::receiver_metrics::Load {
                        retained_bytes: 4_096,
                        retained_pictures: 1,
                        decoding: true,
                        work_us: Some(4_000),
                    }),
                },
                presentation: PresentationMetrics {
                    is_visible: true,
                    presentation_latency_us: Some(3_000),
                },
                application_limited: false,
                route_changed: false,
                reconnected: false,
            };
            let decision = controller.update(snapshot).unwrap();
            ring.record_controller_decision(&decision).unwrap();
        }
        ring.export_bundle(1_500_000, &ProtocolLimits::ABSOLUTE)
            .unwrap()
    }

    #[test]
    fn replay_verifies_deterministic_controller_trace_causes_and_redaction() {
        let dir = temp_dir("roundtrip");
        let file = dir.join("bundle.json");
        let bundle = build_real_controller_bundle();
        let raw_json = bundle.to_sanitized_json().unwrap();
        write_private_regular_file(&file, raw_json.as_bytes()).unwrap();

        let (json_out, exit) = replay(&file, true).unwrap();
        assert_eq!(exit, 0);
        let parsed: serde_json::Value = serde_json::from_str(&json_out).unwrap();
        assert_eq!(parsed["replay_scope"], REPLAY_SCOPE);
        assert_eq!(parsed["deterministic"], true);
        assert_eq!(parsed["steps_replayed"], 4);
        assert_eq!(parsed["matched_steps"], 4);
        assert!(parsed["refusal_code"].is_null());
        assert_eq!(parsed["slowness"]["verdict"], "degraded");

        let (text_out, text_exit) = replay(&file, false).unwrap();
        assert_eq!(text_exit, 0);
        assert!(text_out.contains(REPLAY_SCOPE));
        assert!(text_out.contains("4/4 matched"));
        assert!(text_out.contains("relayed_tailnet_path"));
        assert!(text_out.contains("gpu_to_cpu_copies"));

        // Planted secrets must be absent from exported JSON and both replay outputs.
        for secret in [
            "mac-studio.tail1234.ts.net",
            "fd7a:115c:a1e0::1",
            "/Users/alice/secret.txt",
            "alice@example.com",
            "Payroll 2026",
            "0123456789abcdef0123456789abcdef",
        ] {
            assert!(!raw_json.contains(secret), "leaked {secret} in bundle JSON");
            assert!(!json_out.contains(secret), "leaked {secret} in replay JSON");
            assert!(!text_out.contains(secret), "leaked {secret} in replay text");
        }
    }

    #[test]
    fn tampered_controller_step_returns_typed_mismatch_exit_one() {
        let dir = temp_dir("tamper");
        let file = dir.join("tampered.json");
        let mut bundle = build_real_controller_bundle();
        for ev in &mut bundle.events {
            if let DiagnosticEventKind::ControllerStep { current, .. } = &mut ev.kind {
                current.target_bitrate_bps = current.target_bitrate_bps.saturating_add(123_456);
                break;
            }
        }
        let raw_json = bundle.to_sanitized_json().unwrap();
        write_private_regular_file(&file, raw_json.as_bytes()).unwrap();

        let (json_out, exit) = replay(&file, true).unwrap();
        assert_eq!(exit, 1);
        let parsed: serde_json::Value = serde_json::from_str(&json_out).unwrap();
        assert_eq!(parsed["refusal_code"], "diagnostics_replay_mismatch");
        assert_eq!(parsed["first_mismatch"]["step_index"], 0);
        assert_eq!(parsed["deterministic"], true);
    }

    #[test]
    fn oversized_and_symlink_diagnostic_files_are_refused() {
        let dir = temp_dir("bounds");
        let big = dir.join("too-big.json");
        let oversized = vec![b' '; MAX_IMPORT_JSON_BYTES + 16];
        std::fs::write(&big, &oversized).unwrap();
        assert_eq!(
            replay(&big, true).unwrap_err().code,
            "diagnostics_bundle_too_large"
        );

        #[cfg(unix)]
        {
            let target = dir.join("real.json");
            let link = dir.join("link.json");
            std::fs::write(&target, b"{}").unwrap();
            std::os::unix::fs::symlink(&target, &link).unwrap();
            assert_eq!(
                replay(&link, true).unwrap_err().code,
                "diagnostics_unsafe_path"
            );
            assert_eq!(
                export_session(
                    &link,
                    None,
                    CodecPathKind::Unknown,
                    TransportKind::NativeQuic,
                    None,
                )
                .unwrap_err()
                .code,
                "diagnostics_unsafe_path"
            );
        }
    }

    #[test]
    fn export_without_media_evidence_reports_insufficient_evidence_never_healthy() {
        let dir = temp_dir("empty");
        let file = dir.join("session.json");
        export_session(
            &file,
            None,
            CodecPathKind::Unknown,
            TransportKind::NativeQuic,
            Some("completed for /Users/bob/private host=secret.tail.ts.net"),
        )
        .unwrap();
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(!raw.contains("/Users/bob/private"));
        assert!(!raw.contains("secret.tail.ts.net"));
        let (json_out, exit) = replay(&file, true).unwrap();
        assert_eq!(exit, 0);
        let parsed: serde_json::Value = serde_json::from_str(&json_out).unwrap();
        assert_eq!(parsed["steps_replayed"], 0);
        assert_eq!(parsed["slowness"]["verdict"], "insufficient_evidence");
    }
}
