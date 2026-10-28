//! Pure CLI state and report regressions: no native window, input or network.
use super::*;
use fr_native::desktop::reconnect::CleanupSummary;
use fr_native::viewer_input::StopReason as InputStop;

fn interface() -> Interface {
    Interface {
        display: DisplayChoice::Only,
        progress: Rc::new(RefCell::new(Progress {
            control: Some(control::Counters::default()),
            ..Progress::default()
        })),
        attempt: None,
        clipboard: None,
        audio: None,
    }
}
fn unused_session() -> Session<Interface> {
    Session::new(
        DesktopConfiguration::new(Path::new("/usr/bin/false"), ":0", None, 1).unwrap(),
        ObserverPolicy::default(),
        Mode::Observe,
        interface(),
    )
}
fn transient() -> reconnect::Failure {
    reconnect::Failure::Observation(frd::session_startup::ObserverError::Streaming(
        frd::session_startup::StreamingViewerError::Transport(fr_transport::quic::Error::Expired),
    ))
}
fn reconnecting() -> Status {
    Status::Reconnecting {
        attempt: 2,
        delay: Duration::from_millis(250),
        failure: transient(),
    }
}
fn summary() -> CleanupSummary {
    CleanupSummary {
        disconnect: None,
        control_disconnect: None,
        input_stop: None,
        failure: None,
    }
}
fn requested() -> Interface {
    let mut ui = interface();
    ui.status(Status::Connecting { attempt: 1 }).unwrap();
    ui.progress
        .borrow_mut()
        .control
        .as_mut()
        .unwrap()
        .spend_request();
    ui.status(Status::Cleaning {
        attempt: 1,
        failure: Some(transient()),
    })
    .unwrap();
    ui
}

#[test]
fn a_native_stop_published_during_cleanup_still_vetoes_viewing_reconnect() {
    for (reason, code) in [
        (InputStop::FocusLost, "control_focus_lost"),
        (InputStop::LocalEscape, "control_released_locally"),
        (InputStop::WindowChanged, "control_window_changed"),
    ] {
        let mut ui = requested();
        ui.cleanup_finished(
            1,
            CleanupSummary {
                input_stop: Some(reason),
                ..summary()
            },
        );
        assert_eq!(ui.status(reconnecting()), Err(CallbackError));
        let progress = ui.progress.borrow();
        let failure = completed(
            &unused_session(),
            Err(reconnect::Failure::Notification),
            false,
            &progress,
            true,
        )
        .unwrap_err();
        assert_eq!(failure.code, code);
        let output: serde_json::Value =
            serde_json::from_str(&crate::terminal::output(failure, true)).unwrap();
        assert_eq!(output["control_requested"], true);
        assert_eq!(output["control_request_attempt"], 1);
        // Genuine intent cannot hide an independently failed native cleanup.
        let failure = completed(
            &unused_session(),
            Err(reconnect::Failure::CleanupExpired),
            false,
            &progress,
            true,
        )
        .unwrap_err();
        assert_eq!(failure.code, "native_cleanup_incomplete");
    }
}

#[test]
fn host_revocation_wins_over_a_racing_transport_end_and_keeps_effect_stages() {
    use fr_wire::lease_revoked::{CleanupStage, EffectStage, Reason, Revoked};
    let mut ui = requested();
    let report = Revoked {
        lease: fr_core::ids::InputLeaseId::from_raw(991),
        reason: Reason::LocalRevoke,
        cleanup: CleanupStage::Fenced,
        effects: EffectStage::ReceiptsPending,
    };
    ui.cleanup_finished(
        1,
        CleanupSummary {
            control_disconnect: Some(frd::session_startup::ControlCloseOutcome {
                exchange: frd::session_startup::ViewerCloseOutcome {
                    request_acknowledged: false,
                    report: None,
                    transport: Err(fr_transport::quic::Error::Native),
                },
                revocation: Some(report),
            }),
            ..summary()
        },
    );
    assert_eq!(ui.status(reconnecting()), Err(CallbackError));
    let progress = ui.progress.borrow();
    let error = completed(
        &unused_session(),
        Err(reconnect::Failure::Notification),
        false,
        &progress,
        true,
    )
    .unwrap_err();
    assert_eq!(error.code, "host_control_revoked");
    let output: serde_json::Value =
        serde_json::from_str(&crate::terminal::output(error, true)).unwrap();
    assert_eq!(output["control_lease_report"]["cleanup_stage"], "fenced");
    assert_eq!(
        output["control_lease_report"]["effect_stage"],
        "receipts_pending"
    );
    let text = crate::terminal::output(error, false);
    assert!(text.contains("Original control lease report: reason=local_revoke, cleanup=fenced, effects=receipts_pending"),"{text}");
    assert!(!text.contains("991"));
}

#[test]
fn a_later_view_close_cannot_replace_the_original_control_exchange_or_input_results() {
    use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};
    let mut ui = requested();
    let original = CleanupSummary {
        control_disconnect: Some(frd::session_startup::ControlCloseOutcome {
            exchange: frd::session_startup::ViewerCloseOutcome {
                request_acknowledged: true,
                report: None,
                transport: Err(fr_transport::quic::Error::Native),
            },
            revocation: None,
        }),
        input_stop: Some(InputStop::Local),
        ..summary()
    };
    ui.cleanup_finished(1, original);
    ui.status(reconnecting()).unwrap();
    ui.status(Status::Connecting { attempt: 2 }).unwrap();
    {
        let mut progress = ui.progress.borrow_mut();
        let control = progress.control.as_mut().unwrap();
        assert_eq!(control.opened(), control::AttemptMode::ViewingOnly);
        control.granted = true;
        control.results = 3;
        control.submitted = 2;
    }
    let final_close = frd::session_startup::ViewerCloseOutcome {
        request_acknowledged: true,
        report: Some(Closed {
            reason: ClosedReason::ClientRequested,
            cleanup: Cleanup::Complete,
            effects: OutstandingEffects::Known {
                pending: 0,
                uncertain: 0,
            },
        }),
        transport: Ok(()),
    };
    ui.status(Status::Cleaning {
        attempt: 2,
        failure: None,
    })
    .unwrap();
    ui.cleanup_finished(
        2,
        CleanupSummary {
            disconnect: Some(final_close),
            ..summary()
        },
    );
    let progress = ui.progress.borrow();
    let control = progress.control.unwrap();
    assert_eq!(control.cleanup, Some(original));
    assert_eq!(control.ended, Some(Err(transient())));
    let json = control_completion(&progress, &control, true, Some(final_close.into()), None);
    let output: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(output["last_attempt_mode"], "view_only");
    assert_eq!(output["viewing_reconnects"], 1);
    assert_eq!(output["control_granted"], true); // Historical; no new request.
    assert_eq!(output["input_submitted_to_os"], 2);
    assert_eq!(
        output["control_close_exchange"]["request_acknowledged"],
        true
    );
    assert_eq!(
        output["control_close_exchange"]["transport_completed"],
        false
    );
    assert_eq!(
        output["control_close_exchange"]["host_report"],
        serde_json::Value::Null
    );
    assert_eq!(
        output["close_exchange"]["host_report"]["reason"],
        "client_requested"
    );
    let text = control_completion(&progress, &control, false, Some(final_close.into()), None);
    assert!(
        text.contains("Original control close:") && text.contains("Final connection close:"),
        "{text}"
    );
    assert!(
        text.contains("2 submitted to the host OS")
            && text.contains("Viewing reconnected 1 time(s)"),
        "{text}"
    );
    // A later failed dial also retains the original control effects in errors.
    let error = completed(
        &unused_session(),
        Err(reconnect::Failure::Connection(
            frd::native_connection::Error::Tailnet(TailnetError::LocalApiUnavailable),
        )),
        false,
        &progress,
        true,
    )
    .unwrap_err();
    let output: serde_json::Value =
        serde_json::from_str(&crate::terminal::output(error, true)).unwrap();
    assert_eq!(output["error"]["code"], "tailscale_unavailable");
    assert_eq!(output["input_results"], 3);
    assert_eq!(
        output["control_close_exchange"]["transport_completed"],
        false
    );
}

#[test]
fn an_authenticated_host_close_also_vetoes_a_transport_retry() {
    use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};
    let mut ui = requested();
    ui.cleanup_finished(
        1,
        CleanupSummary {
            disconnect: Some(frd::session_startup::ViewerCloseOutcome {
                request_acknowledged: false,
                report: Some(Closed {
                    reason: ClosedReason::PermissionLost,
                    cleanup: Cleanup::Unconfirmed,
                    effects: OutstandingEffects::Known {
                        pending: 4,
                        uncertain: 2,
                    },
                }),
                transport: Err(fr_transport::quic::Error::Expired),
            }),
            ..summary()
        },
    );
    assert_eq!(ui.status(reconnecting()), Err(CallbackError));
    let progress = ui.progress.borrow();
    let history = control_history(&progress, &progress.control.unwrap());
    assert!(
        history.text.contains("pending=4, uncertain=2"),
        "{}",
        history.text
    );
    assert!(
        history.json.contains("\"pending\":4,\"uncertain\":2"),
        "{}",
        history.json
    );
}

#[test]
fn pending_window_close_stays_terminal_when_transport_failure_wins_the_race() {
    let mut progress = Progress::default();
    progress.begin(1);
    // The WM intent is published before the service loop can handle it. A
    // racing transport failure must not turn that intent into another window.
    assert_eq!(
        progress.before_cleanup(Some(WindowStatus::CloseRequested)),
        Err(CallbackError)
    );
    assert!(progress.user_closed);
    // A later cleanup sample cannot erase already observed user intent.
    for status in [None, Some(WindowStatus::Stopped(StopReason::SessionEnded))] {
        assert_eq!(progress.before_cleanup(status), Err(CallbackError));
        assert!(progress.user_closed);
    }
    let output = completed(&unused_session(), Err(transient()), false, &progress, true).unwrap();
    let output: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(output["outcome"], "stopped");
    let error = completed(
        &unused_session(),
        Err(reconnect::Failure::CleanupExpired),
        false,
        &progress,
        true,
    )
    .unwrap_err();
    assert_eq!(error.code, "native_cleanup_incomplete");

    // Only the explicit start of a fresh attempt resets this per-window latch.
    progress.begin(2);
    assert!(!progress.user_closed);
    assert_eq!(progress.before_cleanup(None), Ok(()));
    assert_eq!(
        completed(&unused_session(), Err(transient()), false, &progress, true)
            .unwrap_err()
            .code,
        "transport_deadline_expired"
    );
}

#[test]
fn late_window_close_after_cleaning_vetoes_retry_without_hiding_cleanup_failure() {
    // The real-window regression proves that cancellation cannot erase or
    // invent the native WM latch. Here its publication is ordered after the
    // actual Cleaning callback, at the same state method used after reap.
    for requested_close in [false, true] {
        let mut ui = requested();
        {
            let mut progress = ui.progress.borrow_mut();
            assert!(!progress.user_closed);
            progress.after_cleanup(requested_close);
        }
        // A subsequent empty sample cannot erase the already retained request.
        ui.cleanup_finished(1, summary());
        assert_eq!(
            ui.status(reconnecting()),
            if requested_close {
                Err(CallbackError)
            } else {
                Ok(())
            }
        );
        let progress = ui.progress.borrow();
        assert_eq!(progress.user_closed, requested_close);
        assert_eq!(progress.control.unwrap().ended, Some(Err(transient())));
        if requested_close {
            let output = completed(
                &unused_session(),
                Err(reconnect::Failure::Notification),
                false,
                &progress,
                true,
            )
            .unwrap();
            let output: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(output["outcome"], "stopped");
        }
        let failure = completed(
            &unused_session(),
            Err(reconnect::Failure::CleanupExpired),
            false,
            &progress,
            true,
        )
        .unwrap_err();
        assert_eq!(failure.code, "native_cleanup_incomplete");
    }
}

#[test]
fn spent_file_consent_is_omitted_without_changing_control_or_original_reports() {
    for clipboard in [false, true] {
        for audio in [false, true] {
            for original in [
                fr_client::native::control_offer_with_auxiliary(clipboard, true, audio),
                fr_client::native::control_offer_with_download(clipboard, audio),
                fr_client::native::control_offer_with_duplex(clipboard, audio),
            ] {
                let mut ui = interface();
                ui.status(Status::Connecting { attempt: 1 }).unwrap();
                assert_eq!(ui.offer(1, &original).unwrap(), original);
                // A pre-request retry has not spent the file selection.
                ui.status(Status::Connecting { attempt: 2 }).unwrap();
                assert_eq!(ui.offer(2, &original).unwrap(), original);
                {
                    let mut progress = ui.progress.borrow_mut();
                    let control = progress.control.as_mut().unwrap();
                    control.spend_request();
                    control.granted = true;
                    control.results = 5;
                    control.submitted = 4;
                }
                ui.status(Status::Cleaning {
                    attempt: 2,
                    failure: Some(transient()),
                })
                .unwrap();
                ui.status(Status::Connecting { attempt: 3 }).unwrap();
                let before = ui.progress.borrow().control.unwrap();
                let narrowed = ui.offer(3, &original).unwrap();
                assert_eq!(ui.progress.borrow().control.unwrap(), before);
                assert_eq!(before.mode, control::AttemptMode::ViewingOnly);
                assert_eq!(before.request_attempt, Some(2));
                assert_eq!(before.submitted, 4);
                assert_eq!(before.ended, Some(Err(transient())));
                assert!(narrowed.validate().is_ok());
                assert_eq!(narrowed.role, fr_wire::negotiation::Role::RequestControl);
                assert_eq!(narrowed.profile, original.profile);
                assert_eq!(narrowed.profile_version, original.profile_version);
                assert_eq!(narrowed.versions, original.versions);
                assert_eq!(narrowed.limits, original.limits);
                for capability in &original.capabilities {
                    let file = [
                        fr_wire::attachment::FILES_CAPABILITY,
                        fr_wire::files::CAPABILITY,
                        fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
                        fr_wire::files::DOWNLOAD_CAPABILITY,
                        fr_wire::files::DUPLEX_CAPABILITY,
                    ]
                    .contains(&capability.name.as_str());
                    assert_eq!(narrowed.capabilities.contains(capability), !file);
                    if capability.required {
                        assert!(!file);
                    }
                }
                assert_eq!(ui.offer(4, &original).unwrap(), narrowed);
            }
        }
    }
}

#[test]
fn observation_reconnect_keeps_its_original_optional_audio_offer() {
    let mut ui = interface();
    ui.progress.borrow_mut().control = None;
    let original = fr_client::native::observation_offer_with_audio();
    assert_eq!(ui.offer(1, &original).unwrap(), original);
    assert_eq!(ui.offer(32, &original).unwrap(), original);
}

fn permission_lost_summary() -> CleanupSummary {
    use fr_wire::closure::{Cleanup, Closed, ClosedReason, OutstandingEffects};
    CleanupSummary {
        disconnect: Some(frd::session_startup::ViewerCloseOutcome {
            request_acknowledged: false,
            report: Some(Closed {
                reason: ClosedReason::PermissionLost,
                cleanup: Cleanup::Unconfirmed,
                effects: OutstandingEffects::Known {
                    pending: 4,
                    uncertain: 2,
                },
            }),
            transport: Err(fr_transport::quic::Error::Expired),
        }),
        ..summary()
    }
}

#[test]
fn a_recovered_viewing_attempt_keeps_its_own_terminal_report_and_vetoes_retry() {
    for requested_control in [false, true] {
        let mut ui = interface();
        if !requested_control {
            ui.progress.borrow_mut().control = None;
        }
        ui.status(Status::Connecting { attempt: 1 }).unwrap();
        if let Some(control) = ui.progress.borrow_mut().control.as_mut() {
            control.spend_request();
        }
        ui.status(Status::Cleaning {
            attempt: 1,
            failure: Some(transient()),
        })
        .unwrap();
        let original = CleanupSummary {
            input_stop: requested_control.then_some(InputStop::Local),
            ..summary()
        };
        ui.cleanup_finished(1, original);
        ui.status(reconnecting()).unwrap();
        ui.status(Status::Connecting { attempt: 2 }).unwrap();
        if let Some(control) = ui.progress.borrow_mut().control.as_mut() {
            assert_eq!(control.opened(), control::AttemptMode::ViewingOnly);
        }
        // The transport result can win the service race, while cleanup still
        // collects an authenticated report from the current viewing session.
        ui.status(Status::Cleaning {
            attempt: 2,
            failure: Some(transient()),
        })
        .unwrap();
        ui.cleanup_finished(2, permission_lost_summary());
        assert_eq!(
            ui.status(Status::Reconnecting {
                attempt: 3,
                delay: Duration::from_millis(500),
                failure: transient(),
            }),
            Err(CallbackError)
        );
        let progress = ui.progress.borrow();
        if let Some(control) = progress.control {
            // The new viewer's report must never rewrite the old controller's
            // request, effects or close exchange just to make the veto work.
            assert_eq!(control.request_attempt, Some(1));
            assert_eq!(control.cleanup, Some(original));
            assert_eq!(control.ended, Some(Err(transient())));
        }
        let error = completed(
            &unused_session(),
            Err(reconnect::Failure::Notification),
            false,
            &progress,
            true,
        )
        .unwrap_err();
        assert_eq!(error.code, "host_permission_lost");
        let output: serde_json::Value =
            serde_json::from_str(&crate::terminal::output(error, true)).unwrap();
        let report = &output["close_exchange"]["host_report"];
        assert_eq!(report["cleanup_stage"], "unconfirmed");
        assert_eq!(report["outstanding_effects"]["pending"], 4);
        assert_eq!(report["outstanding_effects"]["uncertain"], 2);
        assert_eq!(
            completed(
                &unused_session(),
                Err(reconnect::Failure::CleanupExpired),
                false,
                &progress,
                true,
            )
            .unwrap_err()
            .code,
            "native_cleanup_incomplete"
        );
    }
}

#[test]
fn viewing_cleanup_is_attempt_scoped_and_partial_acknowledgements_allow_retry() {
    let mut ui = interface();
    ui.progress.borrow_mut().control = None;
    for (attempt, stop) in [(1, InputStop::Local), (2, InputStop::Closed)] {
        ui.status(Status::Connecting { attempt }).unwrap();
        assert!(ui.progress.borrow().cleanup.is_none());
        // A dial that failed before creating a new desktop may repeat the
        // previous owner's callback. Foreign evidence cannot name this end.
        ui.cleanup_finished(attempt - 1, permission_lost_summary());
        ui.cleanup_finished(attempt + 1, permission_lost_summary());
        assert!(ui.progress.borrow().cleanup.is_none());
        ui.status(Status::Cleaning {
            attempt,
            failure: Some(transient()),
        })
        .unwrap();
        ui.cleanup_finished(
            attempt,
            CleanupSummary {
                disconnect: Some(frd::session_startup::ViewerCloseOutcome {
                    request_acknowledged: true,
                    report: None,
                    transport: Err(fr_transport::quic::Error::Expired),
                }),
                input_stop: Some(stop),
                ..summary()
            },
        );
        assert_eq!(
            ui.status(Status::Reconnecting {
                attempt: attempt + 1,
                delay: Duration::from_millis(250),
                failure: transient(),
            }),
            Ok(())
        );
    }
}
