//! Source authorization is scoped to the original app selection, not settings.
use super::*;
use crate::options::{Command as ClientCommand, Connection};
use fr_wire::sync_job_state::JobMode;

fn parse(args: &[&str]) -> Result<AppCommand, Failure> {
    app_command(&args.iter().map(ToString::to_string).collect::<Vec<_>>())
}

fn pending(recursive: bool) -> Option<Sending> {
    let AppCommand::Open { sending, .. } = parse(&[
        "app",
        "--sync-source",
        "/private/Chosen source 資料",
        if recursive {
            "--sync-tree-job"
        } else {
            "--sync-job"
        },
        "41",
    ])
    .unwrap() else {
        panic!("original app invocation");
    };
    sending
}

fn connection(args: &[String]) -> Connection {
    let ClientCommand::Connect(connection) = crate::options::parse(args).unwrap().command else {
        panic!("ordinary selected viewer");
    };
    connection
}

fn choose(
    source: &mut Option<Sending>,
    settings: &Settings,
    uploads: UploadChoices,
) -> Result<(Vec<String>, Option<u64>), Failure> {
    let mut child_uploads = uploads;
    let selected = sending::take_for_choice(source, &mut child_uploads)?;
    let expected_job = selected.as_ref().map(Sending::job_id);
    let arguments = selected_session_args(
        "nORIGINAL",
        "host.fixture.ts.net",
        settings,
        9443,
        child_uploads,
        false,
    )?;
    Ok((sending::apply(selected, arguments)?, expected_job))
}

#[test]
fn explicit_flat_and_recursive_sources_reach_only_the_selected_controller() {
    for recursive in [false, true] {
        let mut source = pending(recursive);
        let (arguments, expected) = choose(
            &mut source,
            &Settings::default(),
            UploadChoices {
                file_drop: true,
                choose_files: false,
            },
        )
        .unwrap();
        assert!(source.is_none());
        assert_eq!(expected, Some(41));
        let viewer = connection(&arguments);
        assert_eq!(viewer.target.node, "nORIGINAL");
        assert_eq!(viewer.target.port, 9443);
        assert!(
            arguments
                .windows(2)
                .any(|pair| { pair == ["--expect-certificate-name", "host.fixture.ts.net"] })
        );
        let original = viewer.upload.sync_source.unwrap();
        assert_eq!(original.path, PathBuf::from("/private/Chosen source 資料"));
        assert_eq!(original.job_id, 41);
        assert_eq!(
            original.mode,
            if recursive {
                JobMode::Recursive
            } else {
                JobMode::Flat
            },
        );
        assert!(!viewer.upload.file_drop && !viewer.upload.choose_files);
        assert!(!viewer.upload.file_commands && viewer.upload.paths.is_empty());
        assert!(!arguments.iter().any(|arg| arg == "--file-drop"));
        // A fresh physical drop choice on the next picker grants only that
        // new ordinary intake, never the consumed standing source.
        let (next, job) = choose(
            &mut source,
            &Settings::default(),
            UploadChoices {
                file_drop: true,
                choose_files: false,
            },
        )
        .unwrap();
        let viewer = connection(&next);
        assert!(viewer.upload.file_drop && viewer.upload.sync_source.is_none());
        assert_eq!(job, None);
        assert!(!next.iter().any(|arg| arg.contains("Chosen source")));
    }
}

#[test]
fn declining_or_refusing_source_consent_consumes_it_before_any_viewer() {
    for control in [false, true] {
        let mut source = pending(false);
        let settings = Settings {
            control,
            ..Settings::default()
        };
        let (arguments, job) = choose(&mut source, &settings, UploadChoices::default()).unwrap();
        assert!(source.is_none() && job.is_none());
        let viewer = connection(&arguments);
        assert_eq!(viewer.control, control);
        assert!(viewer.upload.sync_source.is_none());
        assert!(!viewer.upload.file_drop);
    }

    let mut source = pending(false);
    let failed = choose(
        &mut source,
        &Settings {
            control: false,
            ..Settings::default()
        },
        UploadChoices {
            file_drop: true,
            choose_files: false,
        },
    )
    .unwrap_err();
    assert_eq!(failed.code, "files_sync_selection_conflict");
    assert!(source.is_none());
    assert!(!format!("{failed:?}").contains("Chosen source"));

    let mut source = pending(true);
    let failed = choose(
        &mut source,
        &Settings::default(),
        UploadChoices {
            file_drop: true,
            choose_files: true,
        },
    )
    .unwrap_err();
    assert_eq!(failed.code, "app_folder_source_invalid");
    assert!(source.is_none());
    // A later successful connection cannot recover either refused choice.
    let (next, job) = choose(&mut source, &Settings::default(), UploadChoices::default()).unwrap();
    assert!(job.is_none() && connection(&next).upload.sync_source.is_none());
}

#[test]
fn unused_picker_refreshes_keep_only_the_local_pending_source_and_explicit_notice() {
    let source = pending(true);
    let mut notice = None;
    let mut failures = 0;
    let mut idle = 0;
    for _ in 0..2 {
        let scoped = sending::notice(source.as_ref(), notice.as_deref()).unwrap();
        assert!(scoped.contains("Upload folder job 41 with nested trees"));
        assert!(scoped.contains("throughout this connection"));
        assert!(scoped.contains("File drops stay disabled"));
        assert!(scoped.contains(sending::consent_label()) && scoped.contains("decline"));
        assert!(!scoped.contains("Chosen source") && scoped.len() <= MAX_APP_NOTICE);
        let arguments = pick_args(&Settings::default(), Some(&scoped), 9443, false);
        let picker = connection(&arguments);
        assert!(picker.upload.sync_source.is_none() && !picker.upload.file_drop);
        assert!(!arguments.iter().any(|arg| arg.contains("Chosen source")));
        assert!(
            retry_picker(
                Some(Report::Failed {
                    code: "host_choice_expired".into(),
                    next: "Refresh the original choice.".into(),
                    unknown_effect: false,
                }),
                &mut notice,
                &mut failures,
                &mut idle,
            )
            .unwrap()
        );
        assert_eq!(source.as_ref().map(Sending::job_id), Some(41));
    }
    let saved = Settings::from_json(
        br#"{"version":1,"sync_source":"/private/old","sync_job":41,"send_files":true}"#,
    )
    .to_json();
    assert!(!saved.contains("sync") && !saved.contains("private") && !saved.contains("send_files"));
}

#[test]
fn upload_job_and_ordinary_download_keep_independent_app_consent() {
    for upload_consent in [false, true] {
        for download_consent in [false, true] {
            let AppCommand::Open {
                mut sending,
                mut receiving,
                ..
            } = parse(&[
                "app",
                "--receive",
                "/private/Original destination",
                "--sync-tree-job",
                "41",
                "--receive-conflict",
                "keep-both",
                "--sync-source",
                "/private/Original source",
            ])
            .unwrap()
            else {
                panic!("original app invocation");
            };
            assert_eq!(receiving.as_ref().and_then(Receiving::sync_job), None);
            let (arguments, job) = choose(
                &mut sending,
                &Settings::default(),
                UploadChoices {
                    file_drop: upload_consent,
                    choose_files: false,
                },
            )
            .unwrap();
            let arguments =
                receiving::take_for_app_choice(&mut receiving, arguments, download_consent)
                    .unwrap();
            assert!(sending.is_none() && receiving.is_none());
            let viewer = connection(&arguments);
            assert_eq!(job, upload_consent.then_some(41));
            assert_eq!(viewer.upload.sync_source.is_some(), upload_consent);
            assert_eq!(viewer.receive.is_some(), download_consent);
            if let Some(receive) = viewer.receive {
                assert_eq!(
                    receive.directory,
                    PathBuf::from("/private/Original destination")
                );
                assert!(receive.keep_both && receive.sync_job.is_none());
            }
            assert!(!viewer.upload.file_drop && !viewer.upload.choose_files);
        }
    }
}

#[test]
fn source_arguments_are_validated_before_settings_discovery_or_source_access() {
    for args in [
        vec!["app", "--sync-source", "/private/source"],
        vec!["app", "--sync-source", "relative", "--sync-job", "41"],
        vec![
            "app",
            "--sync-source",
            "/private/../source",
            "--sync-job",
            "41",
        ],
        vec!["app", "--sync-source", "/private/source", "--sync-job", "0"],
        vec![
            "app",
            "--sync-source",
            "/private/source",
            "--sync-job",
            "41",
            "--sync-tree-job",
            "41",
        ],
        vec![
            "app",
            "--sync-source",
            "/private/source",
            "--sync-job",
            "41",
            "--sync-source",
            "/private/other",
        ],
        vec![
            "app",
            "--sync-source",
            "/private/source",
            "--sync-job",
            "41",
            "--install",
        ],
        vec![
            "app",
            "--sync-source",
            "/private/source",
            "--sync-job",
            "41",
            "--help",
        ],
    ] {
        assert!(parse(&args).is_err(), "{args:?}");
    }
    let invocation = parse(&[
        "app",
        "--port",
        "9443",
        "--sync-source",
        "/private/Secret source",
        "--sync-job",
        "41",
    ])
    .unwrap();
    assert!(!format!("{invocation:?}").contains("Secret source"));
}
