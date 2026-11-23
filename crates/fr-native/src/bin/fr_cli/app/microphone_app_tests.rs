use super::*;

fn command(args: &[&str]) -> Result<AppCommand, Failure> {
    app_command(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

#[test]
fn application_source_is_explicit_bounded_and_never_installed_or_saved() {
    for args in [
        vec!["app", "--microphone-source"],
        vec!["app", "--microphone-server", "/private/pulse"],
        vec![
            "app",
            "--microphone-source",
            "first",
            "--microphone-source",
            "second",
        ],
        vec!["app", "--microphone-source", "first", "--install"],
        vec!["app", "--microphone-source", "first", "--uninstall"],
        vec!["app", "--microphone-source", "first", "--help"],
        vec!["app", "--microphone"],
    ] {
        assert!(command(&args).is_err(), "{args:?}");
    }
    let parsed = command(&[
        "app",
        "--microphone-source",
        "alsa_input.private",
        "--probe",
    ]);
    let supported = cfg!(any(
        all(target_os = "linux", feature = "linux-microphone"),
        all(target_os = "macos", feature = "macos-microphone")
    ));
    if !supported {
        assert_eq!(parsed.unwrap_err().code, "app_microphone_unavailable");
        return;
    }
    let parsed = parsed.unwrap();
    assert!(!format!("{parsed:?}").contains("alsa_input.private"));
    let AppCommand::Open {
        mut microphone,
        probe,
        ..
    } = parsed
    else {
        panic!("app launch expected");
    };
    assert!(probe && microphone::available(microphone.as_ref()));
    let settings = Settings::default();
    let picker = probed_pick_args(&settings, None, 9443, false, probe, true);
    let crate::options::Command::Connect(pick) = crate::options::parse(&picker).unwrap().command
    else {
        panic!("picker expected");
    };
    assert!(pick.pick.unwrap().microphone_available);
    assert!(pick.microphone.is_none());
    assert!(!picker.iter().any(|arg| arg.contains("alsa_input")));
    assert!(!settings.to_json().contains("microphone"));
    // Declining consumes the launch-local source without changing other choices.
    let args = session_args("nOriginal", &settings, 9443, false);
    assert_eq!(
        microphone::take_for_choice(&mut microphone, args.clone(), false).unwrap(),
        args
    );
    assert!(microphone.is_none());
}

#[test]
fn availability_hint_cannot_open_a_device_or_escape_a_selection_only_command() {
    for pick in [false, true] {
        let mut args: Vec<String> = [
            "connect",
            "--choose-host",
            "--control",
            "--experimental-native",
            "--display",
            "only",
        ]
        .map(str::to_owned)
        .into();
        if pick {
            args.push("--pick-only".into());
        }
        args.push("--app-microphone-available".into());
        let parsed = crate::options::parse(&args);
        if !pick {
            assert!(parsed.is_err());
            continue;
        }
        let crate::options::Command::Connect(connection) = parsed.unwrap().command else {
            panic!("selection only expected");
        };
        assert!(connection.pick.as_ref().unwrap().microphone_available);
        assert!(connection.microphone.is_none());
        assert!(
            !crate::host_choice::Choices::initial(&connection)
                .unwrap()
                .microphone_controls
        );
        args.extend(
            ["--microphone", "--microphone-source", "alsa_input.original"].map(str::to_owned),
        );
        assert!(crate::options::parse(&args).is_err());
    }
}

#[cfg(unix)]
#[test]
fn parent_requires_retained_availability_and_a_successful_original_picker() {
    use std::os::unix::process::ExitStatusExt;
    let report = json!({
        "schema_version": 1, "event": "host_selected", "node_id": "nOriginal",
        "certificate_name": "original.tail.example", "port": 9443,
        "control": true, "clipboard": true, "choose_display": false,
        "playback_audio": false, "send_files": false, "receive_files": false,
        "microphone_controls": true, "selection_only": true,
        "fresh_lookup_required": true, "remote_session_started": false,
        "input_requested": false
    });
    for available in [false, true] {
        for success in [false, true] {
            let stdout =
                fr_client::completion::capture_completion(format!("{report}\n").as_bytes());
            let finished = Finished {
                status: std::process::ExitStatus::from_raw(if success { 0 } else { 256 }),
                stdout,
            };
            assert_eq!(
                picker_report_with_microphone(&finished, 9443, false, available).is_some(),
                available && success
            );
            assert!(picker_report_with_microphone(&finished, 8443, false, available).is_none());
        }
    }
}
