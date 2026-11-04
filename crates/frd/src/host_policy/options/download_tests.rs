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
