use super::*;
use crate::options::{Command as ClientCommand, Connection, DisplayChoice};

fn parse(args: &[&str]) -> Result<AppCommand, Failure> {
    app_command(&args.iter().map(ToString::to_string).collect::<Vec<_>>())
}

fn pending(args: &[&str]) -> Option<Receiving> {
    let AppCommand::Open { receiving, .. } = parse(args).unwrap() else {
        panic!("application connection required");
    };
    receiving
}

fn connection(args: &[String]) -> Connection {
    let ClientCommand::Connect(connection) = crate::options::parse(args).unwrap().command else {
        panic!("ordinary viewer command required");
    };
    connection
}

#[test]
fn complete_receive_policy_reaches_only_the_original_selected_viewer() {
    let command = parse(&[
        "app",
        "--json",
        "--port",
        "9443",
        "--receive-conflict",
        "keep-both",
        "--receive-max-session-bytes",
        "8589934592",
        "--receive",
        "/private/local destination",
        "--receive-max-file-bytes",
        "4294967296",
    ])
    .unwrap();
    assert!(!format!("{command:?}").contains("/private/local destination"));
    let AppCommand::Open {
        port,
        mut receiving,
        ..
    } = command
    else {
        panic!("application connection required");
    };
    assert_eq!(port, 9443);
    let settings = Settings {
        control: true,
        clipboard: false,
        choose_display: true,
        playback_audio: true,
        ..Settings::default()
    };
    let args = receiving::take_for_session(
        &mut receiving,
        session_args("nORIGINAL", &settings, port, true),
    )
    .unwrap();
    assert!(receiving.is_none());
    let viewer = connection(&args);
    assert_eq!(viewer.target.node, "nORIGINAL");
    assert_eq!(viewer.target.port, 9443);
    assert!(viewer.control && viewer.upload.file_drop);
    assert!(!viewer.clipboard);
    assert!(viewer.audio.is_some() && viewer.microphone.is_none());
    assert_eq!(viewer.display, DisplayChoice::Choose);
    let receive = viewer.receive.unwrap();
    assert_eq!(
        receive.directory,
        PathBuf::from("/private/local destination")
    );
    assert_eq!(receive.max_file_bytes, Some(4_294_967_296));
    assert_eq!(receive.max_session_bytes, Some(8_589_934_592));
    assert!(receive.keep_both);
    assert_eq!(args.iter().filter(|arg| *arg == "--receive").count(), 1);

    // Launch Services receives exactly one path argument, including spaces.
    let open = open_args(Path::new("/A.app"), Path::new("/out"), true, &args);
    let boundary = open.iter().position(|arg| arg == "--args").unwrap();
    assert_eq!(
        open[boundary + 1..],
        args.iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>()
    );

    // A new picker and another selected host have neither the directory nor
    // its authorization, even when the prior connection failed to open.
    let pick = pick_args(&settings, None, port, receiving.is_some());
    assert!(connection(&pick).receive.is_none());
    let next = receiving::take_for_session(
        &mut receiving,
        session_args("nNEXT", &settings, port, false),
    )
    .unwrap();
    assert!(connection(&next).receive.is_none());
    assert!(!next.iter().any(|arg| arg.starts_with("--receive")));
    assert!(receiving::notice(receiving.as_ref(), None).is_none());
}

#[test]
fn omitted_limits_and_conflict_policy_keep_the_real_receivers_defaults() {
    let mut receiving = pending(&["app", "--receive", "/private/destination"]);
    let args = receiving::take_for_session(
        &mut receiving,
        session_args("nHOST", &Settings::default(), DEFAULT_SERVICE_PORT, false),
    )
    .unwrap();
    let receive = connection(&args).receive.unwrap();
    assert!(receive.max_file_bytes.is_none() && receive.max_session_bytes.is_none());
    assert!(!receive.keep_both);
    assert!(!args.iter().any(|arg| arg == "--receive-conflict"));
    assert!(!args.iter().any(|arg| arg == "--receive-max-file-bytes"));
    assert!(!args.iter().any(|arg| arg == "--receive-max-session-bytes"));
}

#[test]
fn a_view_only_pick_refuses_before_launch_and_consumes_the_receive_choice() {
    let mut receiving = pending(&["app", "--receive", "/private/one-use"]);
    let settings = Settings {
        control: false,
        ..Settings::default()
    };
    let original = session_args("nVIEWER", &settings, 9443, false);
    let error = receiving::take_for_session(&mut receiving, original).unwrap_err();
    assert_eq!(error.code, "receive_requires_control");
    assert!(!format!("{error:?}").contains("/private/one-use"));
    assert!(receiving.is_none());

    // Changing control preferences later cannot recover the refused choice.
    let next = receiving::take_for_session(
        &mut receiving,
        session_args("nOTHER", &Settings::default(), 9443, false),
    )
    .unwrap();
    assert!(connection(&next).receive.is_none());
}

#[test]
fn pending_receive_choice_is_visible_after_refresh_without_exposing_the_path() {
    let receiving = pending(&["app", "--receive", "/private/secret destination"]);
    let notice =
        receiving::notice(receiving.as_ref(), Some("The previous picker expired.")).unwrap();
    assert!(
        notice.starts_with("Receive host files once: choose View and control, then Receive files.")
    );
    assert!(notice.contains("The previous picker expired."));
    assert!(!notice.contains("/private/secret destination"));
    for control in [false, true] {
        let settings = Settings {
            control,
            ..Settings::default()
        };
        let args = pick_args(&settings, Some(&notice), 9443, receiving.is_some());
        assert!(!args.iter().any(|arg| arg.starts_with("--receive")));
        let picker = connection(&args);
        assert!(picker.receive.is_none());
        assert!(picker.pick.is_some());
        assert_eq!(picker.control, control);
        assert_eq!(picker.target.port, 9443);
        assert!(!format!("{receiving:?}").contains("secret"));
    }
    let long = "Previous outcome. ".repeat(MAX_APP_NOTICE);
    let bounded = receiving::notice(receiving.as_ref(), Some(&long)).unwrap();
    assert!(bounded.len() <= MAX_APP_NOTICE);
    assert!(bounded.starts_with("Receive host files once:"));
}

#[test]
fn saved_preferences_cannot_supply_a_destination_or_restore_receiving() {
    let settings = Settings::from_json(
        br#"{"version":1,"receive":"/private/old","receiving":true,"receive_directory":"/private/old","receive_max_file_bytes":1,"receive_conflict":"keep-both"}"#,
    );
    let saved = settings.to_json();
    assert!(!saved.contains("receive") && !saved.contains("/private/old"));
    assert!(pending(&["app"]).is_none());
    let args = session_args("nNEW", &settings, DEFAULT_SERVICE_PORT, false);
    assert!(connection(&args).receive.is_none());
}

#[test]
fn receive_validation_reuses_the_connect_parsers_refusals() {
    for options in [
        vec!["--receive-max-file-bytes", "1"],
        vec!["--receive-max-session-bytes", "2"],
        vec!["--receive-conflict", "keep-both"],
    ] {
        let mut args = vec!["app"];
        args.extend(options);
        assert_eq!(parse(&args).unwrap_err().code, "receive_directory_required");
    }
    for options in [
        vec!["--receive"],
        vec!["--receive", "relative"],
        vec!["--receive", "/private/../escape"],
        vec!["--receive", "/private/\nsecret"],
        vec!["--receive", "/a", "--receive", "/b"],
        vec!["--receive", "/a", "--receive-conflict", "overwrite"],
        vec!["--receive", "/a", "--receive-conflict"],
        vec![
            "--receive",
            "/a",
            "--receive-conflict",
            "reject",
            "--receive-conflict",
            "keep-both",
        ],
        vec![
            "--receive",
            "/a",
            "--receive-max-file-bytes",
            "2",
            "--receive-max-file-bytes",
            "3",
        ],
    ] {
        let mut args = vec!["app"];
        args.extend(options);
        let error = parse(&args).unwrap_err();
        assert_eq!(error.code, "invalid_arguments");
        assert!(!format!("{error:?}").contains("/private/"));
    }
    for value in ["0", "-1", "+1", "1GiB", "18446744073709551616"] {
        for flag in ["--receive-max-file-bytes", "--receive-max-session-bytes"] {
            assert_eq!(
                parse(&["app", "--receive", "/private/destination", flag, value])
                    .unwrap_err()
                    .code,
                "files_limits_invalid"
            );
        }
    }
    assert_eq!(
        parse(&[
            "app",
            "--receive",
            "/private/destination",
            "--receive-max-file-bytes",
            "9",
            "--receive-max-session-bytes",
            "8",
        ])
        .unwrap_err()
        .code,
        "files_limits_invalid"
    );
    let oversized = format!("/{}", "x".repeat(4096));
    assert_eq!(
        parse(&["app", "--receive", &oversized]).unwrap_err().code,
        "invalid_arguments"
    );
}

#[test]
fn receive_consent_cannot_be_installed_or_combined_with_other_app_actions() {
    for action in ["--help", "-h", "--install", "--uninstall"] {
        for args in [
            vec!["app", action, "--receive", "/private/destination"],
            vec!["app", "--receive", "/private/destination", action],
        ] {
            assert_eq!(parse(&args).unwrap_err().code, "invalid_arguments");
        }
    }
    assert!(!desktop_entry("/home/me/.local/bin/fr").contains("--receive"));
    assert!(USAGE.contains("next selected controlling connection only"));
    assert!(USAGE.contains("--receive-conflict POLICY"));
}

#[test]
fn app_shows_the_recursive_scope_and_preserves_it_for_the_original_connection() {
    use fr_wire::sync_job_state::JobMode;
    let mut receiving = pending(&[
        "app", "--receive", "/private/drop", "--receive-conflict", "keep-both", "--sync-tree-job", "41",
    ]);
    let notice = receiving::notice(receiving.as_ref(), None).unwrap();
    assert!(notice.contains("including nested directories"));
    let args = receiving::take_for_app_choice(
        &mut receiving,
        session_args("nORIGINAL", &Settings::default(), 9443, false),
        true,
    ).unwrap();
    assert_eq!(connection(&args).receive.unwrap().sync_job, Some((41, JobMode::Recursive)));
    assert!(receiving.is_none());
}

#[test]
fn explicit_folder_job_reaches_only_the_accepted_original_app_connection() {
    for consent in [false, true] {
        let mut receiving = pending(&[
            "app",
            "--receive",
            "/private/folder destination",
            "--receive-conflict",
            "keep-both",
            "--sync-job",
            "41",
        ]);
        let notice =
            receiving::notice(receiving.as_ref(), Some("Previous viewer closed.")).unwrap();
        assert!(notice.starts_with("Accept host folder job 41:"));
        assert!(notice.contains("throughout this connection"));
        assert!(notice.contains("keeping both on conflicts"));
        assert!(notice.contains("Previous viewer closed."));
        assert!(!notice.contains("/private/folder destination"));
        assert!(!format!("{receiving:?}").contains("/private/folder destination"));

        let picker = pick_args(&Settings::default(), Some(&notice), 9443, true);
        assert!(connection(&picker).receive.is_none());
        assert!(!picker.iter().any(|arg| arg == "--sync-job"));
        let args = receiving::take_for_app_choice(
            &mut receiving,
            session_args("nORIGINAL", &Settings::default(), 9443, false),
            consent,
        )
        .unwrap();
        assert!(receiving.is_none());
        let viewer = connection(&args);
        assert_eq!(viewer.target.node, "nORIGINAL");
        assert!(!viewer.choose_receive_directory);
        if consent {
            let selected = viewer.receive.unwrap();
            assert_eq!(selected.sync_job, Some((41, fr_wire::sync_job_state::JobMode::Flat)));
            assert!(selected.keep_both);
            assert_eq!(
                selected.directory,
                PathBuf::from("/private/folder destination")
            );
        } else {
            assert!(viewer.receive.is_none());
        }
        assert_eq!(
            args.iter().filter(|arg| *arg == "--sync-job").count(),
            usize::from(consent)
        );

        // A failed first launch or a later host choice cannot recover either
        // the job acceptance or its private destination from the app owner.
        let next = receiving::take_for_session(
            &mut receiving,
            session_args("nNEXT", &Settings::default(), 9443, false),
        )
        .unwrap();
        assert!(connection(&next).receive.is_none());
        assert!(!next.iter().any(|arg| arg == "--sync-job"));
        assert!(receiving::notice(receiving.as_ref(), None).is_none());
    }
}

#[test]
fn folder_job_refuses_view_only_and_cannot_be_restored_from_preferences() {
    let mut receiving = pending(&[
        "app",
        "--receive",
        "/private/folder destination",
        "--receive-conflict",
        "keep-both",
        "--sync-job",
        "41",
    ]);
    let settings = Settings {
        control: false,
        ..Settings::default()
    };
    assert!(
        receiving::take_for_app_choice(
            &mut receiving,
            session_args("nVIEW", &settings, 9443, false),
            true,
        )
        .is_err()
    );
    assert!(receiving.is_none());
    let settings = Settings::from_json(
        br#"{"version":1,"sync_job":41,"receive":"/private/old","receive_conflict":"keep-both"}"#,
    );
    let saved = settings.to_json();
    assert!(!saved.contains("sync") && !saved.contains("receive"));
    assert!(
        connection(&session_args("nNEXT", &settings, 9443, false))
            .receive
            .is_none()
    );
}

#[test]
fn app_folder_acceptance_reuses_the_original_job_and_destination_validation() {
    for (options, code) in [
        (vec!["--sync-job", "41"], "files_sync_directory_required"),
        (
            vec!["--receive", "/private/destination", "--sync-job", "41"],
            "files_sync_keep_both_required",
        ),
        (vec!["--sync-job", "0"], "files_sync_job_invalid"),
        (vec!["--sync-job", "-1"], "files_sync_job_invalid"),
        (
            vec!["--sync-job", "41", "--sync-job", "42"],
            "invalid_arguments",
        ),
    ] {
        let mut args = vec!["app"];
        args.extend(options);
        assert_eq!(parse(&args).unwrap_err().code, code);
    }
    for action in ["--help", "--install", "--uninstall"] {
        assert_eq!(
            parse(&["app", "--sync-job", "41", action])
                .unwrap_err()
                .code,
            "invalid_arguments"
        );
    }
    assert!(USAGE.contains("--sync-job ID"));
    assert!(!desktop_entry("/home/me/.local/bin/fr").contains("--sync-job"));
}
