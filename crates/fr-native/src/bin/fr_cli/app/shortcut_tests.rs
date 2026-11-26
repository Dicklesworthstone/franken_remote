//! Fresh picker consent reaches only one actual controlling connection.
use super::*;

fn selected(control: bool, capture_system_shortcuts: bool) -> Value {
    use fr_client::discovery::picker::{Candidate, PathHint};
    let host = Candidate::new(
        "nOriginal".into(),
        "original.fixture.ts.net".into(),
        PathHint::Unknown,
    )
    .unwrap();
    let choices = crate::host_choice::Choices {
        control,
        clipboard: true,
        choose_display: false,
        playback_audio: false,
        send_files: false,
        choose_files: false,
        receive_available: false,
        receive_files: false,
        microphone_controls: false,
        capture_system_shortcuts,
    };
    serde_json::from_str(&crate::host_choice::selected_report(
        &host,
        9443,
        true,
        Some(choices),
    ))
    .unwrap()
}

fn captured(value: &Value) -> CompletionCapture {
    fr_client::completion::capture_completion(value.to_string().as_bytes())
}

#[test]
fn shortcut_consent_is_never_saved_or_preselected_in_another_picker() {
    let settings = Settings::from_json(
        br#"{"version":1,"control":true,"capture_system_shortcuts":true,"system_shortcuts":true}"#,
    );
    let saved: Value = serde_json::from_str(&settings.to_json()).unwrap();
    assert!(saved.get("capture_system_shortcuts").is_none());
    assert!(saved.get("system_shortcuts").is_none());
    for control in [false, true] {
        let settings = Settings {
            control,
            ..settings.clone()
        };
        for _ in 0..2 {
            let args = pick_args(&settings, None, 9443, false);
            assert!(!args.iter().any(|arg| arg == "--capture-system-shortcuts"));
            let crate::options::Command::Connect(connection) =
                crate::options::parse(&args).unwrap().command
            else {
                panic!("original selection-only command");
            };
            assert!(!connection.capture_system_shortcuts);
            assert!(connection.pick.is_some());
            let choices = crate::host_choice::Choices::initial(&connection).unwrap();
            assert!(!choices.capture_system_shortcuts);
        }
    }
}

#[test]
fn only_fresh_supported_control_consent_reaches_the_connect_parser() {
    assert_eq!(
        system_shortcuts_available(),
        cfg!(all(target_os = "macos", feature = "macos-desktop")),
    );
    for control in [false, true] {
        for consent in [false, true] {
            let settings = Settings {
                control,
                ..Settings::default()
            };
            let args = selected_session_args(
                "nOriginal",
                "original.fixture.ts.net",
                &settings,
                9443,
                UploadChoices::default(),
                consent,
            );
            if consent && (!control || !system_shortcuts_available()) {
                assert_eq!(args.unwrap_err().code, "app_system_shortcuts_unavailable");
                continue;
            }
            let args = args.unwrap();
            assert_eq!(
                args.iter()
                    .filter(|arg| *arg == "--capture-system-shortcuts")
                    .count(),
                usize::from(consent),
            );
            let crate::options::Command::Connect(connection) =
                crate::options::parse(&args).unwrap().command
            else {
                panic!("original connection");
            };
            assert_eq!(connection.control, control);
            assert_eq!(connection.capture_system_shortcuts, consent);
            assert!(connection.pick.is_none());
            let selected = crate::saved_hosts::select(&connection.target).unwrap();
            assert!(matches!(
                selected.selector(),
                fr_tailnet::PeerSelector::Pinned {
                    stable_id: "nOriginal",
                    certificate_name: "original.fixture.ts.net"
                }
            ));
            assert_eq!(selected.port, 9443);
            // A later choice starts from the same ordinary preferences, with
            // no system-shortcut consent inherited from this connection.
            let later = selected_session_args(
                "nLater",
                "later.fixture.ts.net",
                &settings,
                9443,
                UploadChoices::default(),
                false,
            )
            .unwrap();
            assert!(!later.iter().any(|arg| arg == "--capture-system-shortcuts"));
            assert!(!settings.to_json().contains("capture_system_shortcuts"));
        }
    }
}

#[test]
fn picker_shortcut_consent_is_typed_and_cannot_request_view_only_capture() {
    for control in [false, true] {
        for consent in [false, true] {
            let report = last_report(&captured(&selected(control, consent)));
            assert_eq!(report.is_some(), !consent || control);
            if let Some(Report::Selected {
                capture_system_shortcuts,
                ..
            }) = report
            {
                assert_eq!(capture_system_shortcuts, consent);
            }
        }
    }
    for invalid in [Value::Null, json!("true"), json!(1), json!({}), json!([])] {
        let mut value = selected(true, false);
        value["capture_system_shortcuts"] = invalid;
        assert_eq!(last_report(&captured(&value)), None);
    }
    let mut missing = selected(true, false);
    missing
        .as_object_mut()
        .unwrap()
        .remove("capture_system_shortcuts");
    assert_eq!(last_report(&captured(&missing)), None);
    assert_eq!(
        last_report(&captured(&json!({"capture_system_shortcuts": true}))),
        None
    );
}

#[cfg(unix)]
#[test]
fn shortcut_consent_needs_the_original_successful_picker_and_actual_mac_build() {
    use std::os::unix::process::ExitStatusExt;
    for consent in [false, true] {
        for success in [false, true] {
            let finished = Finished {
                status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
                stdout: captured(&selected(true, consent)),
            };
            assert_eq!(
                picker_report(&finished, 9443, false).is_some(),
                success && (!consent || system_shortcuts_available()),
            );
            assert_eq!(picker_report(&finished, 8443, false), None);
        }
    }
}
