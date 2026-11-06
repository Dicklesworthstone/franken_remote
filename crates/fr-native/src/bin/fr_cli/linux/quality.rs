//! Content-free host operating-point diagnostics. All values below are
//! historical reports, never an inferred link-health badge or measured FPS.
use fr_wire::quality_decision::{Decision, Mode, Reason};
use frd::session_startup::ViewerStatistics;

fn mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Active => "active",
        Mode::Idle => "idle",
    }
}
fn reason(reason: Reason) -> &'static str {
    match reason {
        Reason::Hold => "hold",
        Reason::SourceWork => "source_work",
        Reason::SendAdmission => "send_admission",
        Reason::CaptureCredit => "capture_credit",
        Reason::ReceiverBacklog => "receiver_backlog",
        Reason::DecoderWork => "decoder_work",
        Reason::HeadroomProbe => "headroom_probe",
        Reason::VerifiedIdle => "verified_idle",
        Reason::ChangedAfterIdle => "changed_after_idle",
        Reason::EvidenceGap => "evidence_gap",
    }
}
fn number(value: Option<u32>) -> String {
    value.map_or_else(|| "null".to_owned(), |value| value.to_string())
}
pub(super) fn json(statistics: &ViewerStatistics) -> String {
    let last = statistics.last_host_quality_decision.map_or_else(
        || "null".to_owned(),
        |decision| {
            format!(
                "{{\"sequence\":{},\"capture_interval_us\":{},\"mode\":\"{}\",\"reason\":\"{}\",\"applied_bitrate_bps\":{},\"pending_bitrate_bps\":{}}}",
                decision.sequence,
                decision.capture_interval_us,
                mode(decision.mode),
                reason(decision.reason),
                number(decision.applied_bitrate_bps),
                number(decision.pending_bitrate_bps),
            )
        },
    );
    format!(
        ",\"host_quality_decisions_received\":{},\"last_host_quality_decision\":{last}",
        statistics.host_quality_decisions_received,
    )
}
pub(super) fn text(statistics: Option<&ViewerStatistics>) -> String {
    let Some(statistics) = statistics else {
        return String::new();
    };
    let Some(Decision {
        sequence,
        capture_interval_us,
        mode: active,
        reason: cause,
        applied_bitrate_bps,
        pending_bitrate_bps,
    }) = statistics.last_host_quality_decision
    else {
        return " Host quality: unknown for the final media view.".to_owned();
    };
    let rate =
        |rate: Option<u32>| rate.map_or_else(|| "unknown".to_owned(), |rate| format!("{rate} bps"));
    let pending = pending_bitrate_bps.map_or_else(
        || "none".to_owned(),
        |rate| format!("{rate} bps (not yet applied)"),
    );
    format!(
        " Last host quality decision #{sequence}: capture interval {capture_interval_us} us, {} ({}); encoder bitrate applied {}, pending {pending}. These are reported settings, not measured delivery rate or view freshness.",
        mode(active),
        reason(cause),
        rate(applied_bitrate_bps),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_absence_is_null_and_never_a_synthetic_healthy_connection() {
        let statistics = ViewerStatistics::default();
        let json = json(&statistics);
        assert_eq!(
            json,
            ",\"host_quality_decisions_received\":0,\"last_host_quality_decision\":null"
        );
        assert!(text(Some(&statistics)).contains("unknown"));
        assert!(text(None).is_empty());
    }

    #[test]
    fn diagnostics_keep_selected_cadence_applied_bitrate_and_pending_target_separate() {
        let statistics = ViewerStatistics {
            host_quality_decisions_received: 4,
            last_host_quality_decision: Some(Decision {
                sequence: 8,
                capture_interval_us: 40_000,
                mode: Mode::Active,
                reason: Reason::SendAdmission,
                applied_bitrate_bps: Some(2_000_000),
                pending_bitrate_bps: Some(1_500_000),
            }),
            ..ViewerStatistics::default()
        };
        let formatted = format!("{{\"existing\":true{}}}", json(&statistics));
        let value: serde_json::Value = serde_json::from_str(&formatted).unwrap();
        let decision = &value["last_host_quality_decision"];
        assert_eq!(value["host_quality_decisions_received"], 4);
        assert_eq!(decision["sequence"], 8);
        assert_eq!(decision["capture_interval_us"], 40_000);
        assert_eq!(decision["mode"], "active");
        assert_eq!(decision["reason"], "send_admission");
        assert_eq!(decision["applied_bitrate_bps"], 2_000_000);
        assert_eq!(decision["pending_bitrate_bps"], 1_500_000);
        assert!(decision.get("fps").is_none());
        assert!(decision.get("bandwidth_bps").is_none());
        let text = text(Some(&statistics));
        assert!(text.contains("applied 2000000 bps"));
        assert!(text.contains("pending 1500000 bps (not yet applied)"));
        assert!(text.contains("not measured delivery rate or view freshness"));
    }

    #[test]
    fn an_idle_source_report_without_bitrate_evidence_does_not_invent_a_rate() {
        let statistics = ViewerStatistics {
            host_quality_decisions_received: 1,
            last_host_quality_decision: Some(Decision {
                sequence: 1,
                capture_interval_us: 200_000,
                mode: Mode::Idle,
                reason: Reason::VerifiedIdle,
                applied_bitrate_bps: None,
                pending_bitrate_bps: None,
            }),
            ..ViewerStatistics::default()
        };
        let value: serde_json::Value =
            serde_json::from_str(&format!("{{\"existing\":true{}}}", json(&statistics))).unwrap();
        assert_eq!(value["last_host_quality_decision"]["mode"], "idle");
        assert_eq!(
            value["last_host_quality_decision"]["reason"],
            "verified_idle"
        );
        assert!(value["last_host_quality_decision"]["applied_bitrate_bps"].is_null());
        assert!(value["last_host_quality_decision"]["pending_bitrate_bps"].is_null());
        assert!(text(Some(&statistics)).contains("applied unknown, pending none"));
    }
}
