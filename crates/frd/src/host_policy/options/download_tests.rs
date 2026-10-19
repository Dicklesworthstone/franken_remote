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
            "/local/file",
            "--files",
            "/local/drop",
        ][..],
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
    assert!(matches!(
        options.validate_file_send(),
        Err(Error::InvalidArgument)
    ));
    options.files = None;
    options.input_agent = None;
    assert!(matches!(
        options.validate_file_send(),
        Err(Error::InvalidArgument)
    ));
}
