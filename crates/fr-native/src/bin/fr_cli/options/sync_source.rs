//! Explicit local source consent for one reverse folder job; never saved or replayed.
use super::Failure;
use fr_wire::sync_job_state::JobMode;
use std::path::{Component, PathBuf};

pub struct FolderSource {
    pub path: PathBuf,
    pub job_id: u64,
    pub mode: JobMode,
}

pub(super) fn accept(
    path: Option<PathBuf>,
    job_id: Option<(u64, JobMode)>,
    control: bool,
    conflicting_selection: bool,
) -> Result<Option<FolderSource>, Failure> {
    let Some(path) = path else {
        return Ok(None);
    };
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err(Failure::new(
            "files_sync_platform_unavailable",
            "Standing folder sources require the native Linux or macOS client.",
            2,
        ));
    }
    if !control || conflicting_selection {
        return Err(Failure::new(
            "files_sync_selection_conflict",
            "A standing upload requires --control and its own --sync-source. Omit ordinary sends, upload pickers, drops and commands. An independently selected ordinary download may share the connection.",
            2,
        ));
    }
    if !path.is_absolute()
        || path.as_os_str().len() > 4096
        || path.components().any(|part| part == Component::ParentDir)
    {
        return Err(Failure::new(
            "files_sync_source_invalid",
            "Select one existing absolute private source directory without parent traversal.",
            2,
        ));
    }
    let (job_id, mode) = job_id.filter(|(id, _)| *id != 0).ok_or_else(|| {
        Failure::new(
            "files_sync_job_required",
            "--sync-source requires the host job ID with --sync-job or --sync-tree-job.",
            2,
        )
    })?;
    Ok(Some(FolderSource { path, job_id, mode }))
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use crate::options::{Command, parse};
    use std::path::Path;

    fn command(extra: &str) -> Result<crate::options::Options, crate::Failure> {
        parse(
            &format!("connect nHOST --experimental-native --display only {extra}")
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn reverse_source_is_an_explicit_controlled_job_without_download_consent() {
        let Command::Connect(connection) =
            command("--control --sync-source /private/source --sync-job 41")
                .unwrap()
                .command
        else {
            panic!("connection");
        };
        assert!(connection.uploads());
        let source = connection.upload.sync_source.as_ref().unwrap();
        assert_eq!(source.path, Path::new("/private/source"));
        assert_eq!(source.job_id, 41);
        assert!(connection.upload.paths.is_empty() && connection.receive.is_none());
        assert!(!connection.upload.file_drop && !connection.upload.file_commands);
    }

    #[test]
    fn recursive_scope_is_explicit_in_both_native_directions() {
        use super::JobMode;
        let Command::Connect(upload) =
            command("--control --sync-source /private/source --sync-tree-job 41")
                .unwrap()
                .command
        else {
            panic!("connection");
        };
        let source = upload.upload.sync_source.unwrap();
        assert_eq!((source.job_id, source.mode), (41, JobMode::Recursive));

        let Command::Connect(download) = command(
            "--control --receive /private/drop --receive-conflict keep-both --sync-tree-job 41",
        )
        .unwrap()
        .command
        else {
            panic!("connection");
        };
        assert_eq!(
            download.receive.unwrap().sync_job,
            Some((41, JobMode::Recursive))
        );
        for invalid in [
            "--control --sync-tree-job 41",
            "--control --sync-source /private/source --sync-job 41 --sync-tree-job 41",
            "--control --sync-source /private/source --sync-tree-job 41 --sync-job 41",
            "--control --receive /private/drop --sync-tree-job 41",
            "--view-only --sync-source /private/source --sync-tree-job 41",
        ] {
            assert!(command(invalid).is_err());
        }
    }

    #[test]
    fn reverse_job_keeps_an_independent_download_choice_and_its_limits() {
        for job_flag in ["--sync-job", "--sync-tree-job"] {
            let Command::Connect(connection) = command(&format!(
                "--control --sync-source /private/source {job_flag} 41 \
                 --receive /private/download --receive-conflict keep-both \
                 --receive-max-file-bytes 1024 --receive-max-session-bytes 4096",
            ))
            .unwrap()
            .command
            else {
                panic!("connection");
            };
            assert!(connection.validate_folder_source().is_ok());
            assert!(connection.uploads());
            let source = connection.upload.sync_source.as_ref().unwrap();
            assert_eq!(source.job_id, 41);
            assert_eq!(
                source.mode,
                super::JobMode::from_recursive(job_flag == "--sync-tree-job")
            );
            let receive = connection.receive.as_ref().unwrap();
            assert_eq!(receive.directory, Path::new("/private/download"));
            assert_eq!(receive.max_file_bytes, Some(1024));
            assert_eq!(receive.max_session_bytes, Some(4096));
            assert!(receive.keep_both);
            assert_eq!(
                receive.sync_job, None,
                "upload consent cannot authorize a download job"
            );
            assert!(!connection.choose_receive_directory);
        }
    }

    #[test]
    fn reverse_job_allows_a_fresh_ordinary_download_picker() {
        let Command::Connect(connection) = command(
            "--control --sync-source /private/source --sync-tree-job 41 --choose-receive-directory",
        )
        .unwrap()
        .command
        else {
            panic!("connection");
        };
        assert!(connection.validate_folder_source().is_ok());
        assert!(connection.uploads() && connection.choose_receive_directory);
        assert!(connection.receive.is_none());
        assert_eq!(connection.upload.sync_source.as_ref().unwrap().job_id, 41);
        // With no source, a job is a receiving job and still needs an explicit
        // keep-both destination. A chooser alone cannot acquire that scope.
        assert!(command("--control --sync-job 41 --choose-receive-directory").is_err());
    }

    #[test]
    fn programmatic_native_options_cannot_discard_another_selection() {
        for conflict in 0..7 {
            let Command::Connect(mut connection) =
                command("--control --sync-source /private/source --sync-job 41")
                    .unwrap()
                    .command
            else {
                panic!("connection");
            };
            assert!(connection.validate_folder_source().is_ok());
            match conflict {
                0 => connection.control = false,
                1 => connection.upload.paths.push("/private/other".into()),
                2 => connection.upload.choose_files = true,
                3 => connection.upload.file_drop = true,
                4 => connection.upload.file_commands = true,
                5 => {
                    let Command::Connect(other) =
                        command("--control --receive /private/destination")
                            .unwrap()
                            .command
                    else {
                        panic!("connection");
                    };
                    connection.receive = other.receive;
                    connection.choose_receive_directory = true;
                }
                _ => {
                    let Command::Connect(other) = command(
                        "--control --receive /private/destination --receive-conflict keep-both --sync-job 42",
                    )
                    .unwrap()
                    .command
                    else {
                        panic!("connection");
                    };
                    connection.receive = other.receive;
                }
            }
            assert!(connection.validate_folder_source().is_err());
        }
    }

    #[test]
    fn reverse_source_cannot_borrow_unrelated_local_selections_or_authority() {
        for extra in [
            "--view-only --sync-source /private/source --sync-job 41",
            "--control --sync-source /private/source",
            "--control --sync-source relative --sync-job 41",
            "--control --sync-source /private/../source --sync-job 41",
            "--control --sync-source /private/source --sync-job 0",
            "--control --sync-source /private/source --sync-job 41 --send /private/file",
            "--control --sync-source /private/source --sync-job 41 --choose-files",
            "--control --sync-source /private/source --sync-job 41 --file-drop",
            "--control --sync-source /private/source --sync-job 41 --file-commands",
            "--control --sync-source /private/source --sync-job 41 --receive /private/destination --choose-receive-directory",
        ] {
            assert!(command(extra).is_err(), "{extra}");
        }
    }
}
