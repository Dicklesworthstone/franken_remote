//! The app retains one configured destination or requests a fresh native choice.
//! A picker result carries consent only; the viewer owns the actual directory.
use super::*;
use crate::options::ReceiveOptions;
use std::os::unix::process::ExitStatusExt;

fn parse(args: &[&str]) -> Result<AppCommand, Failure> {
    app_command(&args.iter().map(ToString::to_string).collect::<Vec<_>>())
}

fn selected(control: bool, receive: bool) -> Value {
    json!({
        "schema_version": 1, "event": "host_selected", "node_id": "nHOST",
        "certificate_name": "host.fixture.ts.net", "port": 9443,
        "selection_only": true, "fresh_lookup_required": true,
        "remote_session_started": false, "input_requested": false,
        "control": control, "clipboard": true, "choose_display": true,
        "playback_audio": false, "send_files": false, "receive_files": receive,
        "microphone_controls": false,
    })
}

fn finished(value: &Value, success: bool) -> Finished {
    Finished {
        status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
        stdout: fr_client::completion::capture_completion_with_limit(
            value.to_string().as_bytes(),
            fr_client::completion::CompletionLimit::NativeCommand,
        ),
    }
}

fn receive() -> ReceiveOptions {
    ReceiveOptions {
        directory: PathBuf::from("/private/Chosen folder 資料"),
        max_file_bytes: Some(4 * 1024 * 1024),
        max_session_bytes: Some(8 * 1024 * 1024),
        keep_both: true,
    }
}

fn pending() -> Option<Receiving> {
    let AppCommand::Open { receiving, .. } = parse(&[
        "app",
        "--receive",
        "/private/Chosen folder 資料",
        "--receive-max-file-bytes",
        "4194304",
        "--receive-max-session-bytes",
        "8388608",
        "--receive-conflict",
        "keep-both",
    ])
    .unwrap() else {
        panic!("original app invocation");
    };
    receiving
}

fn connection(args: &[String]) -> crate::options::Connection {
    let crate::options::Command::Connect(connection) = crate::options::parse(args).unwrap().command
    else {
        panic!("original viewer command");
    };
    connection
}

#[test]
fn ordinary_app_receiving_requires_a_fresh_local_choice_for_every_viewer() {
    let native = receiving::directory_picker_available();
    assert_eq!(
        native,
        cfg!(any(
            all(target_os = "linux", feature = "linux-desktop"),
            all(target_os = "macos", feature = "macos-desktop")
        ))
    );
    assert_eq!(receiving::available(None), native);
    assert!(receiving::available(pending().as_ref()));
    for consent in [false, true, false, true] {
        let mut destination = None;
        let result = receiving::take_for_app_choice(
            &mut destination,
            session_args("nHOST", &Settings::default(), 9443, false),
            consent,
        );
        assert!(destination.is_none());
        if consent && !native {
            assert_eq!(result.unwrap_err().code, "invalid_arguments");
            continue;
        }
        let args = result.unwrap();
        let viewer = connection(&args);
        assert_eq!(viewer.choose_receive_directory, consent);
        assert!(viewer.receive.is_none());
        assert_eq!(
            args.iter()
                .filter(|arg| *arg == "--choose-receive-directory")
                .count(),
            usize::from(consent)
        );
        let picker = connection(&pick_args(&Settings::default(), None, 9443, native));
        assert!(!picker.choose_receive_directory && picker.receive.is_none());
        let choices = crate::host_choice::Choices::initial(&picker).unwrap();
        assert_eq!(choices.receive_available, native);
        assert!(!choices.receive_files);
    }
    let saved = Settings::from_json(
        br#"{"version":1,"choose_receive_directory":true,"receive_files":true,"receive":"/private/old"}"#,
    )
    .to_json();
    assert!(!saved.contains("receive") && !saved.contains("private"));
}

#[test]
fn configured_destination_is_consumed_before_a_later_fresh_directory_choice() {
    for consent in [false, true] {
        let mut destination = pending();
        let args = receiving::take_for_app_choice(
            &mut destination,
            session_args("nFIRST", &Settings::default(), 9443, false),
            consent,
        )
        .unwrap();
        let first = connection(&args);
        assert_eq!(first.receive, consent.then(receive));
        assert!(!first.choose_receive_directory);
        assert!(destination.is_none());
        let args = receiving::take_for_app_choice(
            &mut destination,
            session_args("nNEXT", &Settings::default(), 9443, false),
            false,
        )
        .unwrap();
        let next = connection(&args);
        assert!(next.receive.is_none() && !next.choose_receive_directory);
        assert!(!args.iter().any(|arg| arg.contains("Chosen folder")));
    }
    if receiving::directory_picker_available() {
        let mut destination = None;
        let view = Settings {
            control: false,
            ..Settings::default()
        };
        assert_eq!(
            receiving::take_for_app_choice(
                &mut destination,
                session_args("nHOST", &view, 9443, false),
                true,
            )
            .unwrap_err()
            .code,
            "receive_requires_control"
        );
    }
}

#[test]
#[cfg(any(
    all(target_os = "linux", feature = "linux-desktop"),
    all(target_os = "macos", feature = "macos-desktop")
))]
fn actual_prestart_receive_summary_keeps_cancel_and_local_failures_retryable() {
    for code in [
        "receive_directory_choice_cancelled",
        "receive_directory_choice_expired",
        "receive_directory_choice_invalid",
        "files_directory_writable_by_others",
        "runtime_unavailable",
    ] {
        let mut failure = Failure::new(code, "No connection was made.", 2);
        crate::file_reports::download::Summary::unavailable("files_receive_not_started")
            .preserve(&mut failure);
        let value: Value = serde_json::from_str(&crate::terminal::output(failure, true)).unwrap();
        assert_eq!(value["files_receive_requested"], true);
        assert_eq!(value["files_receive_cleanup"], "not_started");
        assert_eq!(value["files_received"], json!([]));
        assert!(
            session_notice_checked(&finished(&value, false).stdout, "host", false, true).is_ok(),
            "{code}"
        );
    }
    let mut failure = Failure::new(
        "receive_directory_picker_cleanup_incomplete",
        "Inspect the original picker.",
        1,
    );
    crate::file_reports::download::Summary::unavailable("files_receive_not_started")
        .preserve(&mut failure);
    let value: Value = serde_json::from_str(&crate::terminal::output(failure, true)).unwrap();
    assert!(
        session_notice_checked(&finished(&value, false).stdout, "host", false, true)
            .unwrap_err()
            .unknown_effect
    );
}

#[test]
fn launch_receive_policy_reuses_direct_connect_validation_without_opening_a_root() {
    let AppCommand::Open {
        port,
        mut receiving,
        ..
    } = parse(&[
        "app",
        "--port",
        "9443",
        "--receive",
        "/private/Chosen folder 資料",
        "--receive-max-file-bytes",
        "4194304",
        "--receive-max-session-bytes",
        "8388608",
        "--receive-conflict",
        "keep-both",
    ])
    .unwrap()
    else {
        panic!("original launch configuration");
    };
    assert_eq!(port, 9443);
    let args = receiving::take_for_choice(
        &mut receiving,
        session_args("nHOST", &Settings::default(), port, false),
        true,
    )
    .unwrap();
    assert!(receiving.is_none());
    let policy = connection(&args).receive.unwrap();
    assert_eq!(policy, receive());
    assert!(!format!("{policy:?}").contains("Chosen folder"));
    assert!(
        !format!(
            "{:?}",
            parse(&["app", "--receive", "/private/secret"]).unwrap()
        )
        .contains("secret")
    );
    let AppCommand::Open { mut receiving, .. } =
        parse(&["app", "--receive", "/private/missing-destination"]).unwrap()
    else {
        panic!("parsing does not access the destination");
    };
    let args = receiving::take_for_choice(
        &mut receiving,
        session_args("nHOST", &Settings::default(), 9443, false),
        true,
    )
    .unwrap();
    let policy = connection(&args).receive.unwrap();
    assert_eq!(policy.max_file_bytes, None);
    assert_eq!(policy.max_session_bytes, None);
    assert!(!policy.keep_both);
}

#[test]
fn invalid_launch_receive_policy_is_rejected_before_a_picker_opens() {
    for args in [
        vec!["app", "--receive", "relative"],
        vec!["app", "--receive", "/private/../other"],
        vec!["app", "--receive", "/private/secret\0"],
        vec!["app", "--receive"],
        vec!["app", "--receive", "/private/secret", "--receive", "/other"],
        vec!["app", "--receive", "/private/secret", "--install"],
        vec!["app", "--uninstall", "--receive", "/private/secret"],
        vec!["app", "--help", "--receive", "/private/secret"],
        vec![
            "app",
            "--receive",
            "/private/secret",
            "--receive-conflict",
            "overwrite",
        ],
        vec![
            "app",
            "--receive",
            "/private/secret",
            "--receive-conflict",
            "reject",
            "--receive-conflict",
            "keep-both",
        ],
        vec![
            "app",
            "--receive",
            "/private/secret",
            "--receive-max-file-bytes",
            "4",
            "--receive-max-file-bytes",
            "4",
        ],
        vec![
            "app",
            "--receive",
            "/private/secret",
            "--receive-max-session-bytes",
            "8",
            "--receive-max-session-bytes",
            "8",
        ],
        vec!["app", "--app-receive-available"],
    ] {
        let failure = parse(&args).unwrap_err();
        assert_eq!(failure.code, "invalid_arguments", "{args:?}");
        assert!(!format!("{failure:?}").contains("secret"));
    }
    for (flag, value) in [
        ("--receive-max-file-bytes", "0"),
        ("--receive-max-session-bytes", "18446744073709551616"),
    ] {
        assert_eq!(
            parse(&["app", "--receive", "/private/secret", flag, value])
                .unwrap_err()
                .code,
            "files_limits_invalid"
        );
    }
    for args in [
        vec!["app", "--receive-max-file-bytes", "4"],
        vec!["app", "--receive-max-session-bytes", "8"],
        vec!["app", "--receive-conflict", "keep-both"],
    ] {
        assert_eq!(parse(&args).unwrap_err().code, "receive_directory_required");
    }
    assert_eq!(
        parse(&[
            "app",
            "--receive",
            "/private/secret",
            "--receive-max-file-bytes",
            "9",
            "--receive-max-session-bytes",
            "8"
        ])
        .unwrap_err()
        .code,
        "files_limits_invalid"
    );
}

#[test]
fn only_a_confirmed_controller_can_opt_into_the_original_available_destination() {
    for control in [false, true] {
        for available in [false, true] {
            for consent in [false, true] {
                for success in [false, true] {
                    let report = picker_report(
                        &finished(&selected(control, consent), success),
                        9443,
                        available,
                    );
                    assert_eq!(
                        report.is_some(),
                        success && (!consent || (control && available))
                    );
                }
            }
        }
    }
    for invalid in [Value::Null, json!("true"), json!(1), json!({})] {
        let mut value = selected(true, false);
        value["receive_files"] = invalid;
        assert_eq!(picker_report(&finished(&value, true), 9443, true), None);
    }
    let mut missing = selected(true, false);
    missing.as_object_mut().unwrap().remove("receive_files");
    assert_eq!(picker_report(&finished(&missing, true), 9443, true), None);
    assert_eq!(
        picker_report(&finished(&selected(true, true), true), 8443, true),
        None
    );
}

#[test]
fn pickers_hold_only_availability_and_every_new_choice_starts_without_consent() {
    let mut settings = Settings::from_json(br#"{"version":1,"receive":"/private/secret","receive_files":true,"receive_available":true,"receive_max_file_bytes":1}"#);
    let saved = settings.to_json();
    assert!(!saved.contains("receive") && !saved.contains("secret"));
    for control in [false, true] {
        settings.control = control;
        for available in [false, true] {
            let args = pick_args(&settings, None, 9443, available);
            assert_eq!(
                args.iter()
                    .filter(|arg| *arg == "--app-receive-available")
                    .count(),
                usize::from(available)
            );
            assert!(
                !args
                    .iter()
                    .any(|arg| arg == "--receive" || arg.contains("private"))
            );
            let crate::options::Command::Connect(connection) =
                crate::options::parse(&args).unwrap().command
            else {
                panic!("selection-only child");
            };
            assert!(connection.receive.is_none());
            let choices = crate::host_choice::Choices::initial(&connection).unwrap();
            assert_eq!(choices.receive_available, available);
            assert!(!choices.receive_files);
            assert_eq!(connection.target.port, 9443);
        }
    }
    for args in [
        "connect nHOST --control --experimental-native --display only --app-receive-available",
        "connect --choose-host --control --experimental-native --display only --app-receive-available",
        "connect --choose-host --pick-only --control --experimental-native --display only --app-receive-available --receive /private/secret",
        "connect --choose-host --pick-only --control --experimental-native --display only --app-receive-available --app-receive-available",
    ] {
        assert_eq!(
            crate::options::parse(
                &args
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            )
            .err()
            .unwrap()
            .code,
            "invalid_arguments"
        );
    }
}

#[test]
fn confirmed_receiving_reaches_the_existing_owner_with_original_limits_and_conflict_policy() {
    let original = receive();
    for control in [false, true] {
        for consent in [false, true] {
            for send_files in [false, true] {
                let settings = Settings {
                    control,
                    ..Settings::default()
                };
                let mut receiving = pending();
                let args = receiving::take_for_choice(
                    &mut receiving,
                    session_args("nHOST", &settings, 9443, send_files),
                    control && consent,
                )
                .unwrap();
                assert!(receiving.is_none());
                let crate::options::Command::Connect(connection) =
                    crate::options::parse(&args).unwrap().command
                else {
                    panic!("original viewer command");
                };
                assert_eq!(connection.target.port, 9443);
                assert_eq!(
                    connection.receive,
                    (control && consent).then(|| original.clone())
                );
                assert_eq!(connection.upload.file_drop, control && send_files);
                assert!(connection.pick.is_none());
                assert!(!args.iter().any(|arg| arg == "--app-receive-available"));
            }
        }
    }
    // An untrusted child cannot replace any launch-scoped policy via extra
    // report fields; only the explicit Boolean is consumed.
    let mut value = selected(true, true);
    value["receive"] = json!("/attacker/destination");
    value["receive_max_file_bytes"] = json!(u64::MAX);
    value["receive_conflict"] = json!("overwrite");
    let Some(Report::Selected {
        node,
        receive_files,
        ..
    }) = picker_report(&finished(&value, true), 9443, true)
    else {
        panic!("original destination is available");
    };
    let args = receiving::take_for_choice(
        &mut pending(),
        session_args(&node, &Settings::default(), 9443, false),
        receive_files,
    )
    .unwrap();
    assert!(
        !args
            .iter()
            .any(|arg| arg.contains("attacker") || arg == "overwrite")
    );
    let crate::options::Command::Connect(connection) =
        crate::options::parse(&args).unwrap().command
    else {
        panic!("original viewer command");
    };
    assert_eq!(connection.receive, Some(original));
}

#[test]
#[cfg(any(
    all(target_os = "linux", feature = "linux-desktop"),
    all(target_os = "macos", feature = "macos-desktop")
))]
fn invalid_effective_receive_caps_refuse_before_app_discovery_or_root_access() {
    let defaults = frd::native_files::Limits::default();
    for (flag, value) in [
        ("--receive-max-file-bytes", defaults.max_session_bytes + 1),
        ("--receive-max-session-bytes", defaults.max_file_bytes - 1),
    ] {
        assert_eq!(
            parse(&[
                "app",
                "--receive",
                "/private/nonexistent",
                flag,
                &value.to_string()
            ])
            .unwrap_err()
            .code,
            "files_limits_invalid"
        );
    }
}

#[test]
fn only_confirmed_receiving_requires_disk_owner_completion_after_the_viewer() {
    for consent in [false, true] {
        let mut receiving = pending();
        let args = receiving::take_for_choice(
            &mut receiving,
            session_args("nHOST", &Settings::default(), 9443, false),
            consent,
        )
        .unwrap();
        assert!(receiving.is_none());
        assert_eq!(connection(&args).receive.is_some(), consent);
        let mut report = json!({
            "schema_version": 1, "outcome": "stopped", "role": "control",
            "cleanup_confirmed": true,
        });
        let ordinary = session_notice_checked(
            &finished(&report, true).stdout,
            "workstation",
            true,
            consent,
        );
        if consent {
            assert_eq!(ordinary.unwrap_err().code, "app_receive_outcome_unknown");
        } else {
            assert!(ordinary.is_ok());
        }
        report.as_object_mut().unwrap().extend(
            json!({
                "files_receive_requested": consent,
                "files_received": [], "files_receive_refused": [],
                "files_receive_absence": null, "files_receive_ended": null,
                "files_receive_cleanup": if consent { "finished" } else { "not_started" },
                "files_receive_progress": null, "files_receive_results_truncated": false,
                "files_receive_cancel_requested": false,
            })
            .as_object()
            .unwrap()
            .clone(),
        );
        assert!(
            session_notice_checked(
                &finished(&report, true).stdout,
                "workstation",
                true,
                consent
            )
            .is_ok()
        );
    }
}

#[test]
fn declining_or_failing_the_first_choice_never_restores_receiving() {
    for consent in [false, true] {
        let mut receiving = pending();
        let first = receiving::take_for_choice(
            &mut receiving,
            session_args("nFIRST", &Settings::default(), 9443, false),
            consent,
        )
        .unwrap();
        assert_eq!(connection(&first).receive.is_some(), consent);
        assert!(receiving.is_none());
        let next = session_args("nNEXT", &Settings::default(), 9443, false);
        assert_eq!(
            receiving::take_for_choice(&mut receiving, next.clone(), true)
                .unwrap_err()
                .code,
            "invalid_arguments"
        );
        assert!(
            connection(&receiving::take_for_choice(&mut receiving, next, false).unwrap())
                .receive
                .is_none()
        );
    }
    let mut receiving = pending();
    let invalid_role = Settings {
        control: false,
        ..Settings::default()
    };
    let error = receiving::take_for_choice(
        &mut receiving,
        session_args("nBAD", &invalid_role, 9443, false),
        true,
    )
    .unwrap_err();
    assert_eq!(error.code, "receive_requires_control");
    assert!(receiving.is_none());
}
