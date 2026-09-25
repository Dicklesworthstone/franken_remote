//! Content-free projection of an authenticated terminal control report.
//! No lease ID, ticket, input value or peer-provided string reaches CLI output.
//! Reported cleanup and effect accounting remain independent; neither is rollback.
use super::Failure;
use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason, Revoked};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Report {
    reason: Reason,
    cleanup: CleanupStage,
    effects: EffectStage,
}
impl Report {
    pub(super) const fn reason(self) -> &'static str {
        self.reason.code()
    }
    pub(super) const fn cleanup(self) -> &'static str {
        match self.cleanup {
            CleanupStage::Fenced => "fenced",
            CleanupStage::Released => "released",
            CleanupStage::Failed => "failed",
        }
    }
    pub(super) const fn effects(self) -> &'static str {
        match self.effects {
            EffectStage::Unknown => "unknown",
            EffectStage::ReceiptsPending => "receipts_pending",
            EffectStage::ReceiptsComplete => "receipts_complete",
        }
    }
    const fn code(self) -> &'static str {
        match self.reason {
            Reason::LocalRevoke => "host_control_revoked",
            Reason::LeaseExpired => "host_control_lease_expired",
            Reason::ObservationEnded => "host_observation_ended",
            Reason::ViewInvalidated => "host_view_invalidated",
            Reason::SessionEnded => "host_session_ended",
            Reason::PermissionLost => "host_permission_lost",
            Reason::HostFailure => "host_control_failure",
            Reason::ClientRequested => "host_control_released",
            Reason::Suspended => "host_session_suspended",
        }
    }
}

/// Call only after the original controlled viewer validated the exact session,
/// control route and lease. Ordinary disconnects do not imply this report.
pub(super) fn failure(value: Revoked) -> Failure {
    let report = Report {
        reason: value.reason,
        cleanup: value.cleanup,
        effects: value.effects,
    };
    Failure {
        code: report.code(),
        next: "Control has ended; it will not be reacquired automatically. Check the host-reported cleanup and effect status before starting a new session. Previously submitted actions are not rolled back.",
        exit: 1,
        revocation: Some(report),
    }
}

/// Terminal reports have their own outcome. Generic failures keep the existing
/// output contract unchanged and never infer a host reason from connection loss.
pub(super) fn output(error: Failure, json: bool) -> String {
    let Some(report) = error.revocation else {
        return crate::output::failure(error, json);
    };
    if json {
        use crate::output::{quoted, timestamp};
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"revoked\",\"error\":{{\"code\":{},\"next_action\":{},\"revocation\":{{\"reason\":{},\"cleanup_stage\":{},\"effect_stage\":{}}}}}}}\n",
            timestamp(),
            quoted(error.code),
            quoted(error.next),
            quoted(report.reason()),
            quoted(report.cleanup()),
            quoted(report.effects())
        )
    } else {
        format!(
            "{}: {} Host report: reason={}, cleanup={}, effects={}.\n",
            error.code,
            error.next,
            report.reason(),
            report.cleanup(),
            report.effects()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::ids::InputLeaseId;

    #[test]
    fn all_reasons_and_independent_stages_reach_json_and_text_without_identifiers() {
        let reasons = [
            (Reason::LocalRevoke, "host_control_revoked", "local_revoke"),
            (
                Reason::LeaseExpired,
                "host_control_lease_expired",
                "lease_expired",
            ),
            (
                Reason::ObservationEnded,
                "host_observation_ended",
                "observation_ended",
            ),
            (
                Reason::ViewInvalidated,
                "host_view_invalidated",
                "view_invalidated",
            ),
            (Reason::SessionEnded, "host_session_ended", "session_ended"),
            (
                Reason::PermissionLost,
                "host_permission_lost",
                "permission_lost",
            ),
            (Reason::HostFailure, "host_control_failure", "host_failure"),
            (
                Reason::ClientRequested,
                "host_control_released",
                "client_requested",
            ),
            (Reason::Suspended, "host_session_suspended", "suspended"),
        ];
        for (reason, code, reason_name) in reasons {
            for (cleanup, cleanup_name) in [
                (CleanupStage::Fenced, "fenced"),
                (CleanupStage::Released, "released"),
                (CleanupStage::Failed, "failed"),
            ] {
                for (effects, effect_name) in [
                    (EffectStage::Unknown, "unknown"),
                    (EffectStage::ReceiptsPending, "receipts_pending"),
                    (EffectStage::ReceiptsComplete, "receipts_complete"),
                ] {
                    let value = Revoked {
                        lease: InputLeaseId::from_raw(0xdead_beef_cafe_babe),
                        reason,
                        cleanup,
                        effects,
                    };
                    let projected = failure(value);
                    assert_eq!(projected.code, code);
                    assert_eq!(projected.exit, 1);
                    // Even changing the full lease ID cannot change this projection.
                    assert_eq!(
                        projected,
                        failure(Revoked {
                            lease: InputLeaseId::from_raw(7),
                            ..value
                        })
                    );
                    let encoded = output(projected, true);
                    let json: serde_json::Value = serde_json::from_str(&encoded).unwrap();
                    assert_eq!(json["outcome"], "revoked");
                    assert_eq!(json["error"]["code"], code);
                    let report = &json["error"]["revocation"];
                    assert_eq!(report["reason"], reason_name);
                    assert_eq!(report["cleanup_stage"], cleanup_name);
                    assert_eq!(report["effect_stage"], effect_name);
                    assert_eq!(report.as_object().unwrap().len(), 3);
                    let text = output(projected, false);
                    for expected in [code, reason_name, cleanup_name, effect_name] {
                        assert!(text.contains(expected), "{text}");
                    }
                    assert!(!encoded.contains("deadbeef"));
                    assert!(projected.next.contains("not rolled back"));
                }
            }
        }
    }

    #[test]
    fn absent_reports_do_not_invent_revocation_or_cleanup_success() {
        for (exit, outcome) in [(1, "refused"), (130, "cancelled")] {
            let error = Failure::new("session_failed", "Inspect host state.", exit);
            let json: serde_json::Value = serde_json::from_str(&output(error, true)).unwrap();
            assert_eq!(json["outcome"], outcome);
            assert!(json["error"].get("revocation").is_none());
            assert_eq!(
                output(error, false),
                "session_failed: Inspect host state.\n"
            );
        }
    }
}
