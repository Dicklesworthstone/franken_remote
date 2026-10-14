//! The real CLI error adapters and renderer, given validated typed reports.
//! No network authentication, native cleanup or external effect is fabricated.
use super::*;
use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};
use frd::native_connection::reconnect::{Failure as ReconnectFailure, retryable};

fn report(reason: ClosedReason) -> Closed {
    Closed {
        reason,
        cleanup: Cleanup::Unconfirmed,
        effects: OutstandingEffects::Unknown,
    }
}
fn failures(error: session_startup::Error) -> [ReconnectFailure; 4] {
    [
        ReconnectFailure::Connection(native_connection::Error::Session(error)),
        ReconnectFailure::Observation(ObserverError::Session(error)),
        ReconnectFailure::Observation(ObserverError::Streaming(StreamingViewerError::Session(
            error,
        ))),
        ReconnectFailure::Observation(ObserverError::Streaming(StreamingViewerError::Control(
            ControlledViewerError::Session(error),
        ))),
    ]
}

#[test]
fn every_host_closed_reason_reaches_cli_through_observation_control_and_reconnect() {
    let cases = [
        (ClosedReason::ClientRequested, "host_session_closed"),
        (ClosedReason::HostStopping, "host_stopping"),
        (ClosedReason::AuthorityExpired, "host_authority_expired"),
        (ClosedReason::PermissionLost, "host_permission_lost"),
        (ClosedReason::ViewInvalidated, "host_view_invalidated"),
        (ClosedReason::ProtocolError, "host_protocol_error"),
        (ClosedReason::HostFailure, "host_failure"),
        (ClosedReason::SessionReplaced, "host_session_replaced"),
    ];
    let mut codes = std::collections::BTreeSet::new();
    for (reason, code) in cases {
        assert!(codes.insert(code));
        let error = session_startup::Error::RemoteClosed(report(reason));
        let expected = session(error).unwrap();
        assert_eq!(expected.code, code);
        assert_eq!(expected.exit, 1);
        assert!(expected.revocation.is_none());
        for failure in failures(error) {
            assert_eq!(reconnect(failure), Some(expected));
            assert!(!retryable(failure), "host closure must not reacquire a session");
        }
        let rendered = crate::terminal::output(expected, true);
        let json: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(json["error"]["code"], code);
        assert_eq!(json["outcome"], "refused");
        let exchange = &json["close_exchange"];
        assert_eq!(exchange["request_acknowledged"], false);
        assert_eq!(exchange["transport_completed"], false);
        assert_eq!(exchange["host_report"]["reason"], reason.code());
        assert!(json["error"].get("revocation").is_none());
        let text = crate::terminal::output(expected, false);
        assert!(text.starts_with(code));
        assert!(text.contains(reason.code()));
        assert!(text.contains("not rolled back"));
    }
}

#[test]
fn unsolicited_closed_preserves_unknown_zero_and_uncertain_effects_independently() {
    for (cleanup, name) in [
        (Cleanup::Unconfirmed, "unconfirmed"),
        (Cleanup::Complete, "complete"),
        (Cleanup::Incomplete, "incomplete"),
    ] {
        for effects in [
            OutstandingEffects::Unknown,
            OutstandingEffects::Known {
                pending: 0,
                uncertain: 0,
            },
            OutstandingEffects::Known {
                pending: 3,
                uncertain: u32::MAX,
            },
        ] {
            let error = session(session_startup::Error::RemoteClosed(Closed {
                cleanup,
                effects,
                ..report(ClosedReason::HostFailure)
            }))
            .unwrap();
            let json: serde_json::Value =
                serde_json::from_str(&crate::terminal::output(error, true)).unwrap();
            let host = &json["close_exchange"]["host_report"];
            assert_eq!(host["cleanup_stage"], name);
            let counts = &host["outstanding_effects"];
            match effects {
                OutstandingEffects::Unknown => {
                    assert_eq!(counts["known"], false);
                    assert!(counts["pending"].is_null());
                    assert!(counts["uncertain"].is_null());
                }
                OutstandingEffects::Known { pending, uncertain } => {
                    assert_eq!(counts["known"], true);
                    assert_eq!(counts["pending"], pending);
                    assert_eq!(counts["uncertain"], uncertain);
                }
            }
            assert_eq!(error.exit, 1, "reported cleanup is not local success");
            assert_eq!(host.as_object().unwrap().len(), 3);
            assert!(json.get("cleanup_confirmed").is_none());
        }
    }
}

#[test]
fn a_local_close_or_transport_failure_never_invents_a_host_closed_report() {
    use fr_transport::quic::Error as Transport;
    for error in [
        session_startup::Error::Closed,
        session_startup::Error::Cancelled,
        session_startup::Error::Expired,
        session_startup::Error::Transport(Transport::Closed),
        session_startup::Error::Transport(Transport::Expired),
        session_startup::Error::Transport(Transport::Native),
        session_startup::Error::Transport(Transport::Unauthorized),
    ] {
        for failure in failures(error) {
            assert_eq!(reconnect(failure), None, "{failure:?}");
        }
    }
}

#[cfg(feature = "linux-desktop")]
#[test]
fn an_empty_later_close_exchange_cannot_erase_the_original_host_report() {
    for (exit, code, outcome) in [
        (1, "native_cleanup_incomplete", "refused"),
        (130, "cancelled", "cancelled"),
    ] {
        let original = session(session_startup::Error::RemoteClosed(Closed {
            cleanup: Cleanup::Incomplete,
            effects: OutstandingEffects::Known {
                pending: 2,
                uncertain: 9,
            },
            ..report(ClosedReason::HostFailure)
        }))
        .unwrap();
        let mut local = Failure::new(code, "Inspect remaining native owners.", exit);
        local.closure = original.closure;
        for acknowledged in [false, true] {
            let result = crate::closure::preserve::<()>(
                Err(local),
                Some(session_startup::ViewerCloseOutcome {
                    request_acknowledged: acknowledged,
                    report: None,
                    transport: Err(fr_transport::quic::Error::Expired),
                }),
            )
            .unwrap_err();
            assert_eq!((result.code, result.exit), (code, exit));
            let json: serde_json::Value =
                serde_json::from_str(&crate::terminal::output(result, true)).unwrap();
            assert_eq!(json["outcome"], outcome);
            let exchange = &json["close_exchange"];
            assert_eq!(exchange["request_acknowledged"], acknowledged);
            assert_eq!(exchange["transport_completed"], false);
            assert_eq!(exchange["host_report"]["reason"], "host_failure");
            assert_eq!(exchange["host_report"]["cleanup_stage"], "incomplete");
            assert_eq!(exchange["host_report"]["outstanding_effects"]["pending"], 2);
            assert_eq!(exchange["host_report"]["outstanding_effects"]["uncertain"], 9);
        }
    }
}
