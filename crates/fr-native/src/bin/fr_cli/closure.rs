//! Bounded content-free closing evidence, never an inferred remote cleanup result.
use super::Failure;
use fr_wire::closure::{Cleanup, Closed, OutstandingEffects};
use frd::session_startup::ViewerCloseOutcome;

/// Keep exact peer stages but project transport errors to completion, not raw
/// diagnostics or identifiers. A received report survives a failed ACK flush.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Report {
    request_acknowledged: bool,
    transport_completed: bool,
    host: Option<Closed>,
}
impl From<ViewerCloseOutcome> for Report {
    fn from(outcome: ViewerCloseOutcome) -> Self {
        Self {
            request_acknowledged: outcome.request_acknowledged,
            transport_completed: outcome.transport.is_ok(),
            host: outcome.report,
        }
    }
}
impl Report {
    /// Called by the existing terminal-output owner. The original failure and
    /// its exit code remain authoritative even when host cleanup was reported.
    pub(super) fn failure(self, error: Failure, json: bool) -> String {
        if json {
            use crate::output::{quoted, timestamp};
            format!(
                "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":{},\"error\":{{\"code\":{},\"next_action\":{}}},\"close_exchange\":{}}}\n",
                timestamp(),
                quoted(if error.exit == 130 {
                    "cancelled"
                } else {
                    "refused"
                }),
                quoted(error.code),
                quoted(error.next),
                self.json(),
            )
        } else {
            format!("{}: {} {}\n", error.code, error.next, self.text())
        }
    }
    pub(super) fn json(self) -> String {
        let report = self.host.map_or_else(
            || "null".to_owned(),
            |report| {
                let (known, pending, uncertain) = match report.effects {
                    OutstandingEffects::Unknown => (false, "null".to_owned(), "null".to_owned()),
                    OutstandingEffects::Known { pending, uncertain } => {
                        (true, pending.to_string(), uncertain.to_string())
                    }
                };
                format!(
                    "{{\"reason\":\"{}\",\"cleanup_stage\":\"{}\",\"outstanding_effects\":{{\"known\":{known},\"pending\":{pending},\"uncertain\":{uncertain}}}}}",
                    report.reason.code(),
                    cleanup(report.cleanup),
                )
            },
        );
        format!(
            "{{\"request_acknowledged\":{},\"transport_completed\":{},\"host_report\":{report}}}",
            self.request_acknowledged, self.transport_completed,
        )
    }
    pub(super) fn text(self) -> String {
        let host = self.host.map_or_else(
            || {
                "No host Closed report; host cleanup and outstanding effects are unknown."
                    .to_owned()
            },
            |report| {
                let effects = match report.effects {
                    OutstandingEffects::Unknown => "unknown".to_owned(),
                    OutstandingEffects::Known { pending, uncertain } => {
                        format!("pending={pending}, uncertain={uncertain}")
                    }
                };
                format!(
                    "Host-reported reason={}, cleanup={}, outstanding effects={effects}.",
                    report.reason.code(),
                    cleanup(report.cleanup),
                )
            },
        );
        format!(
            "Close request transport acknowledgement: {}; closing transport: {}. {host}",
            if self.request_acknowledged {
                "received"
            } else {
                "not confirmed"
            },
            if self.transport_completed {
                "completed"
            } else {
                "not completed"
            },
        )
    }
}
const fn cleanup(stage: Cleanup) -> &'static str {
    match stage {
        Cleanup::Unconfirmed => "unconfirmed",
        Cleanup::Complete => "complete",
        Cleanup::Incomplete => "incomplete",
    }
}

/// Local cleanup failure/cancellation must keep any already-collected host
/// report. The failure code and exit status still win; no success is fabricated.
#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
pub(super) fn preserve<T>(
    result: Result<T, Failure>,
    outcome: Option<ViewerCloseOutcome>,
) -> Result<T, Failure> {
    result.map_err(|mut error| {
        if let Some(outcome) = outcome {
            error.closure = Some(Report::from(outcome));
        }
        error
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_transport::quic::Error;
    use fr_wire::closure::ClosedReason;

    fn outcome(report: Option<Closed>) -> ViewerCloseOutcome {
        ViewerCloseOutcome {
            request_acknowledged: true,
            report,
            transport: Ok(()),
        }
    }
    #[test]
    fn all_peer_stages_remain_exact_without_identifiers_or_free_form_errors() {
        for reason in [
            ClosedReason::ClientRequested,
            ClosedReason::HostStopping,
            ClosedReason::AuthorityExpired,
            ClosedReason::PermissionLost,
            ClosedReason::ViewInvalidated,
            ClosedReason::ProtocolError,
            ClosedReason::HostFailure,
            ClosedReason::SessionReplaced,
        ] {
            for stage in [Cleanup::Unconfirmed, Cleanup::Complete, Cleanup::Incomplete] {
                for effects in [
                    OutstandingEffects::Unknown,
                    OutstandingEffects::Known {
                        pending: 0,
                        uncertain: 0,
                    },
                    OutstandingEffects::Known {
                        pending: 2,
                        uncertain: u32::MAX,
                    },
                ] {
                    let report = Report::from(outcome(Some(Closed {
                        reason,
                        cleanup: stage,
                        effects,
                    })));
                    let json: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
                    let host = &json["host_report"];
                    assert_eq!(host["reason"], reason.code());
                    assert_eq!(host["cleanup_stage"], cleanup(stage));
                    match effects {
                        OutstandingEffects::Unknown => {
                            assert_eq!(host["outstanding_effects"]["known"], false);
                            assert!(host["outstanding_effects"]["pending"].is_null());
                            assert!(host["outstanding_effects"]["uncertain"].is_null());
                        }
                        OutstandingEffects::Known { pending, uncertain } => {
                            assert_eq!(host["outstanding_effects"]["known"], true);
                            assert_eq!(host["outstanding_effects"]["pending"], pending);
                            assert_eq!(host["outstanding_effects"]["uncertain"], uncertain);
                        }
                    }
                    assert_eq!(json.as_object().unwrap().len(), 3);
                    assert_eq!(host.as_object().unwrap().len(), 3);
                    assert!(report.json().len() < 512);
                    let text = report.text();
                    assert!(text.contains("Host-reported"));
                    assert!(text.contains(reason.code()) && text.contains(cleanup(stage)));
                }
            }
        }
    }
    #[test]
    fn request_acknowledgement_without_report_is_not_cleanup_or_effect_confirmation() {
        let mut value = outcome(None);
        value.transport = Err(Error::Expired);
        let report = Report::from(value);
        let json: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        assert_eq!(json["request_acknowledged"], true);
        assert_eq!(json["transport_completed"], false);
        assert!(json["host_report"].is_null());
        assert!(
            report
                .text()
                .contains("host cleanup and outstanding effects are unknown")
        );
    }
    #[test]
    fn report_survives_failed_ack_flush_and_ack_stages_do_not_depend_on_each_other() {
        let report = Some(Closed {
            reason: ClosedReason::ClientRequested,
            cleanup: Cleanup::Complete,
            effects: OutstandingEffects::Known {
                pending: 3,
                uncertain: 7,
            },
        });
        for acknowledged in [false, true] {
            for transport in [Ok(()), Err(Error::Unauthorized)] {
                let value = Report::from(ViewerCloseOutcome {
                    request_acknowledged: acknowledged,
                    report,
                    transport,
                });
                let json: serde_json::Value = serde_json::from_str(&value.json()).unwrap();
                assert_eq!(json["request_acknowledged"], acknowledged);
                assert_eq!(json["transport_completed"], transport.is_ok());
                assert_eq!(json["host_report"]["cleanup_stage"], "complete");
                assert_eq!(json["host_report"]["outstanding_effects"]["pending"], 3);
                assert_eq!(json["host_report"]["outstanding_effects"]["uncertain"], 7);
            }
        }
    }
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    #[test]
    fn local_failure_keeps_host_effects_without_changing_failure_exit_or_outcome() {
        for (code, exit, outcome_name) in [
            ("native_cleanup_incomplete", 1, "refused"),
            ("cancelled", 130, "cancelled"),
        ] {
            let error = Failure::new(code, "Inspect remaining native owners.", exit);
            let report = outcome(Some(Closed {
                reason: ClosedReason::HostStopping,
                cleanup: Cleanup::Incomplete,
                effects: OutstandingEffects::Known {
                    pending: 2,
                    uncertain: 9,
                },
            }));
            let preserved = preserve::<()>(Err(error), Some(report)).unwrap_err();
            assert_eq!((preserved.code, preserved.exit), (code, exit));
            let json: serde_json::Value =
                serde_json::from_str(&crate::terminal::output(preserved, true)).unwrap();
            assert_eq!(json["outcome"], outcome_name);
            assert_eq!(json["error"]["code"], code);
            assert_eq!(
                json["close_exchange"]["host_report"]["cleanup_stage"],
                "incomplete"
            );
            assert_eq!(
                json["close_exchange"]["host_report"]["outstanding_effects"]["uncertain"],
                9
            );
            assert!(json.get("cleanup_confirmed").is_none());
            let text = crate::terminal::output(preserved, false);
            assert!(text.starts_with(code) && text.contains("pending=2, uncertain=9"));
            assert_eq!(preserve::<()>(Err(error), None).unwrap_err(), error);
        }
    }
}
