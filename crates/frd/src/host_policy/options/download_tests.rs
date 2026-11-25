use super::{Error, RunOptions};
use std::path::PathBuf;

fn parse(args: &[&str]) -> Result<RunOptions, Error> {
    RunOptions::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
}

#[test]
fn download_selection_is_explicit_bounded_and_requires_one_controlled_share() {
    let options = parse(&[
        "--once",
        "--input-agent",
        "/local/input-agent",
        "--send",
        "/local/first",
        "--send",
        "/local/tree",
        "--clipboard",
        "--audio",
    ])
    .unwrap();
    assert_eq!(
        options.send,
        [PathBuf::from("/local/first"), PathBuf::from("/local/tree")]
    );
    assert!(options.clipboard && options.audio && options.once);
    assert!(options.files.is_none());
    assert_eq!(parse(&[]).unwrap().send, [] as [std::path::PathBuf; 0]);
    for args in [
        &["--send", "/local/file"][..],
        &["--once", "--send", "/local/file"][..],
        &["--input-agent", "/local/agent", "--send", "/local/file"][..],
        &[
            "--once",
            "--input-agent",
            "/local/agent",
            "--send",
            "relative",
        ][..],
        &[
            "--once",
            "--input-agent",
            "/local/agent",
            "--send",
            "/local/../file",
        ][..],
        &["--once", "--input-agent", "/local/agent", "--send"][..],
    ] {
        assert!(
            matches!(parse(args), Err(Error::InvalidArgument)),
            "{args:?}"
        );
    }
    let mut many = vec!["--once", "--input-agent", "/local/agent"];
    for _ in 0..=crate::native_files::MAX_SENDS {
        many.extend(["--send", "/local/file"]);
    }
    assert!(matches!(parse(&many), Err(Error::InvalidArgument)));
}

#[test]
fn programmatic_download_options_keep_the_same_scope_and_conflict_checks() {
    let mut options = RunOptions {
        once: true,
        input_agent: Some(PathBuf::from("/local/agent")),
        send: vec![PathBuf::from("/local/file")],
        ..RunOptions::default()
    };
    assert!(options.validate_file_send().is_ok());
    options.once = false;
    assert!(matches!(
        options.validate_file_send(),
        Err(Error::InvalidArgument)
    ));
    options.once = true;
    options.files = Some(PathBuf::from("/local/drop"));
    assert!(options.validate_file_send().is_ok());
    options.files = None;
    options.input_agent = None;
    assert!(matches!(
        options.validate_file_send(),
        Err(Error::InvalidArgument)
    ));
}

#[test]
fn in_process_control_allows_only_an_explicit_bounded_one_share_selection() {
    // Exercise the Windows executor choice through the production validator
    // on every test platform. Native parsing and handles have their separate
    // Windows integration target; no file or policy store is opened here.
    let source = std::env::temp_dir().join("selected-host-file.txt");
    let mut options = RunOptions {
        once: true,
        control: true,
        send: vec![source.clone()],
        ..RunOptions::default()
    };
    assert!(options.input_agent.is_none());
    assert!(options.validate_file_send().is_ok());
    for approval in [super::Approval::None, super::Approval::Local] {
        assert_eq!(options.windows_refusal(approval), None);
    }
    options.once = false;
    assert!(options.validate_file_send().is_err());
    options.once = true;
    options.control = false;
    assert!(options.validate_file_send().is_err());
    options.control = true;
    options.send = vec![source; crate::native_files::MAX_SENDS];
    assert!(options.validate_file_send().is_ok());
    options
        .send
        .push(std::env::temp_dir().join("one-too-many.txt"));
    assert!(options.validate_file_send().is_err());
    for path in [
        PathBuf::from("relative.txt"),
        std::env::temp_dir()
            .join("parent")
            .join("..")
            .join("file.txt"),
    ] {
        options.send = vec![path];
        assert!(options.validate_file_send().is_err());
    }
    options.send.clear();
    options.send_control = Some("desk".into());
    assert!(options.validate_file_send().is_err());
}

#[test]
fn host_can_select_a_source_and_drop_directory_for_one_controlled_share() {
    let options = parse(&[
        "--once",
        "--input-agent",
        "/local/agent",
        "--send",
        "/local/file",
        "--files",
        "/local/drop",
    ])
    .unwrap();
    assert_eq!(options.send, [PathBuf::from("/local/file")]);
    assert_eq!(options.files, Some(PathBuf::from("/local/drop")));
    assert!(options.once);
}

#[test]
fn live_host_send_endpoint_requires_one_explicit_controlled_share() {
    let options = parse(&[
        "--once",
        "--input-agent",
        "/local/agent",
        "--send-control",
        "desk-1",
        "--files",
        "/local/drop",
        "--audio",
    ])
    .unwrap();
    assert_eq!(options.send_control.as_deref(), Some("desk-1"));
    assert_eq!(options.send.len(), 0);
    assert!(options.clipboard && options.audio);
    assert_eq!(options.files, Some(PathBuf::from("/local/drop")));
    for args in [
        vec!["--send-control", "desk"],
        vec!["--once", "--send-control", "desk"],
        vec!["--input-agent", "/local/agent", "--send-control", "desk"],
        vec![
            "--once",
            "--input-agent",
            "/local/agent",
            "--send-control",
            "desk",
            "--send",
            "/local/file",
        ],
        vec![
            "--once",
            "--input-agent",
            "/local/agent",
            "--send-control",
            "desk/other",
        ],
        vec![
            "--once",
            "--input-agent",
            "/local/agent",
            "--send-control",
            "",
        ],
        vec![
            "--once",
            "--input-agent",
            "/local/agent",
            "--send-control",
            "desk",
            "--send-control",
            "other",
        ],
    ] {
        assert!(
            matches!(parse(&args), Err(Error::InvalidArgument)),
            "{args:?}"
        );
    }
}

#[test]
fn programmatic_live_send_options_cannot_bypass_original_scope() {
    let mut options = RunOptions {
        once: true,
        input_agent: Some(PathBuf::from("/local/agent")),
        send_control: Some("desk".into()),
        ..RunOptions::default()
    };
    assert!(options.validate_file_send().is_ok());
    options.send.push(PathBuf::from("/local/file"));
    assert!(options.validate_file_send().is_err());
    options.send.clear();
    options.once = false;
    assert!(options.validate_file_send().is_err());
    options.once = true;
    options.input_agent = None;
    assert!(options.validate_file_send().is_err());
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn standing_folder_source_requires_an_explicit_original_controlled_share() {
    let options = parse(&[
        "--once",
        "--input-agent",
        "/local/agent",
        "--sync-source",
        "/local/source",
        "--sync-job",
        "41",
    ])
    .unwrap();
    assert_eq!(options.sync_source, Some(PathBuf::from("/local/source")));
    assert_eq!(options.sync_job, Some(41));
    assert!(options.send.is_empty() && options.send_control.is_none());
    assert!(parse(&[]).unwrap().sync_source.is_none());
    for extra in [
        vec!["--sync-source", "/local/source"],
        vec!["--sync-job", "41"],
        vec!["--sync-source", "/local/source", "--sync-job", "0"],
        vec!["--sync-source", "relative", "--sync-job", "41"],
        vec!["--sync-source", "/local/../source", "--sync-job", "41"],
        vec![
            "--sync-source",
            "/local/source",
            "--sync-job",
            "41",
            "--sync-job",
            "42",
        ],
        vec![
            "--sync-source",
            "/local/source",
            "--sync-job",
            "41",
            "--send",
            "/local/file",
        ],
        vec![
            "--sync-source",
            "/local/source",
            "--sync-job",
            "41",
            "--send-control",
            "desk",
        ],
    ] {
        let mut args = vec!["--once", "--input-agent", "/local/agent"];
        args.extend(extra);
        assert!(
            matches!(parse(&args), Err(Error::InvalidArgument)),
            "{args:?}"
        );
    }
    let mut missing_scope = options;
    missing_scope.once = false;
    assert!(missing_scope.validate_file_send().is_err());
    missing_scope.once = true;
    missing_scope.input_agent = None;
    assert!(missing_scope.validate_file_send().is_err());
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reverse_folder_destination_requires_explicit_keep_both_and_one_controlled_share() {
    let options = parse(&[
        "--once",
        "--input-agent",
        "/local/agent",
        "--sync-destination",
        "/local/destination",
        "--sync-job",
        "41",
        "--files-conflict",
        "keep-both",
    ])
    .unwrap();
    assert_eq!(
        options.sync_destination,
        Some(PathBuf::from("/local/destination"))
    );
    assert_eq!(options.sync_job, Some(41));
    assert!(options.files.is_none() && options.sync_source.is_none());
    assert!(options.send.is_empty() && options.send_control.is_none());
    assert!(options.validate_file_send().is_ok());

    for extra in [
        vec!["--files", "/local/another"],
        vec!["--sync-source", "/local/source"],
        vec!["--send", "/local/file"],
        vec!["--send-control", "desk"],
    ] {
        let mut args = vec![
            "--once",
            "--input-agent",
            "/local/agent",
            "--sync-destination",
            "/local/destination",
            "--sync-job",
            "41",
            "--files-conflict",
            "keep-both",
        ];
        args.extend(extra);
        assert!(parse(&args).is_err(), "{args:?}");
    }
    let mut invalid = options;
    invalid.files_conflict = None;
    assert!(invalid.validate_file_send().is_err());
    assert!(invalid.open_files().is_err());
    invalid.files_conflict = Some(super::ConflictPolicy::KeepBoth);
    invalid.once = false;
    assert!(invalid.validate_file_send().is_err());
    invalid.once = true;
    invalid.input_agent = None;
    assert!(invalid.validate_file_send().is_err());
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn reverse_folder_options_reject_missing_id_and_unsafe_source_scope() {
    for extra in [
        vec!["--sync-destination", "/local/destination"],
        vec![
            "--sync-destination",
            "/local/destination",
            "--sync-job",
            "0",
        ],
        vec!["--sync-destination", "relative", "--sync-job", "41"],
        vec![
            "--sync-destination",
            "/local/../destination",
            "--sync-job",
            "41",
        ],
    ] {
        let mut args = vec![
            "--once",
            "--input-agent",
            "/local/agent",
            "--files-conflict",
            "keep-both",
        ];
        args.extend(extra);
        assert!(parse(&args).is_err(), "{args:?}");
    }
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn tree_jobs_require_a_single_explicit_host_scope_in_either_direction() {
    use fr_wire::sync_job_state::JobMode;
    for (direction, extra) in [
        ("--sync-source", &[][..]),
        ("--sync-destination", &["--files-conflict", "keep-both"][..]),
    ] {
        let mut args = vec!["--once", "--input-agent", "/local/agent", direction, "/local/folder"];
        args.extend_from_slice(extra);
        args.extend(["--sync-tree-job", "41"]);
        let selected = parse(&args).unwrap();
        assert_eq!(selected.sync_job, Some(41));
        assert_eq!(selected.sync_job_mode, JobMode::Recursive);
        args.extend(["--sync-job", "41"]);
        assert!(parse(&args).is_err());
    }
}
