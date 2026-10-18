//! Installation carries exactly the host operator's local collision policy.
//! Rendering/reparsing is not a claim that systemd has activated the service.
use super::*;
use crate::{
    host_policy::options::RunOptions, native_files::ConflictPolicy,
    service_install::render_systemd_unit,
};

fn install(tail: &[&str]) -> Result<InstallOptions, ServiceError> {
    let args = ["--user", "--software-explicit", "--dry-run", "--"]
        .into_iter()
        .chain(tail.iter().copied())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    InstallOptions::parse_cli(&args)
}
fn emitted(options: &InstallOptions) -> RunOptions {
    let mut options = options.clone();
    options.exec_path = PathBuf::from("/usr/libexec/frd");
    let unit = render_systemd_unit(&options);
    // Fixtures contain no whitespace in values; systemd quoting has separate
    // coverage. Parse the emitted run arguments, not the original input vector.
    let args = unit
        .lines()
        .find_map(|line| line.strip_prefix("ExecStart=:"))
        .unwrap()
        .split_whitespace()
        .skip(2)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    RunOptions::parse(&args).unwrap()
}

#[test]
fn selected_and_default_file_policies_survive_the_rendered_service() {
    for (name, expected) in [
        (None, None),
        (Some("reject"), Some(ConflictPolicy::Reject)),
        (Some("keep-both"), Some(ConflictPolicy::KeepBoth)),
    ] {
        let mut args = vec![
            "--input-agent",
            "/input",
            "--files",
            "/drop",
            "--files-max-file-bytes",
            "3",
            "--files-max-session-bytes",
            "6",
        ];
        if let Some(name) = name {
            args.extend(["--files-conflict", name]);
        }
        let options = install(&args).unwrap();
        let run = emitted(&options);
        assert_eq!(run.files_conflict, expected);
        assert_eq!(
            run.file_conflict_policy().unwrap(),
            expected.unwrap_or_default()
        );
        assert_eq!(run.files, Some(PathBuf::from("/drop")));
        assert_eq!(run.files_max_file_bytes, Some(3));
        assert_eq!(run.files_max_session_bytes, Some(6));
        assert_eq!(run.input_agent, Some(PathBuf::from("/input")));
        assert!(run.approval.is_none() && run.sharing.is_none());
        assert!(!run.audio && !run.clipboard);
    }
}

#[test]
fn installer_cannot_turn_conflict_preference_into_receive_or_control_permission() {
    assert!(matches!(
        install(&["--files-conflict", "keep-both"]),
        Err(ServiceError::InvalidOptions)
    ));
    assert!(matches!(
        install(&["--files", "/drop", "--files-conflict", "keep-both"]),
        Err(ServiceError::HostProfileUnavailable {
            code: "files_requires_input_agent",
            ..
        })
    ));
    for policy in ["overwrite", "merge", "replace", "keep-both "] {
        assert!(matches!(
            install(&[
                "--input-agent",
                "/input",
                "--files",
                "/drop",
                "--files-conflict",
                policy
            ]),
            Err(ServiceError::InvalidOptions)
        ));
    }
    assert!(matches!(
        install(&[
            "--input-agent",
            "/input",
            "--files",
            "/drop",
            "--files-conflict",
            "reject",
            "--files-conflict",
            "keep-both"
        ]),
        Err(ServiceError::InvalidOptions)
    ));
}
