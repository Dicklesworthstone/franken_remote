//! Local receive policy, fixed before any connection or disk worker starts.
//! The host's manifest can never choose these limits or collision behavior.
use super::{Failure, PathBuf, usage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiveOptions {
    pub directory: PathBuf,
    pub max_file_bytes: Option<u64>,
    pub max_session_bytes: Option<u64>,
    pub keep_both: bool,
}

fn invalid_limits() -> Failure {
    Failure::new(
        "files_limits_invalid",
        "Receive limits must be positive byte counts, with the session limit at least the per-file limit. Nothing was received and no connection was made.",
        2,
    )
}

pub(super) fn bytes(value: &str) -> Result<u64, Failure> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid_limits());
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or_else(invalid_limits)
}

pub(super) fn keep_both(value: &str) -> Result<bool, Failure> {
    match value {
        "reject" => Ok(false),
        "keep-both" => Ok(true),
        _ => Err(usage()),
    }
}

impl ReceiveOptions {
    pub(super) fn new(
        directory: Option<PathBuf>,
        max_file_bytes: Option<u64>,
        max_session_bytes: Option<u64>,
        keep_both: Option<bool>,
    ) -> Result<Option<Self>, Failure> {
        let Some(directory) = directory else {
            if max_file_bytes.is_some() || max_session_bytes.is_some() || keep_both.is_some() {
                return Err(Failure::new(
                    "receive_directory_required",
                    "Receive limits and conflict policy require an explicit --receive directory on a controlling connection.",
                    2,
                ));
            }
            return Ok(None);
        };
        if max_file_bytes == Some(0)
            || max_session_bytes == Some(0)
            || matches!((max_file_bytes, max_session_bytes), (Some(file), Some(session)) if session < file)
        {
            return Err(invalid_limits());
        }
        Ok(Some(Self {
            directory,
            max_file_bytes,
            max_session_bytes,
            keep_both: keep_both.unwrap_or(false),
        }))
    }

    /// Resolve omitted limits from the actual native receiver, never copied CLI
    /// defaults. Invalid effective limits refuse before looking up the directory.
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    pub(crate) fn open(&self) -> Result<frd::native_files::Directory, Failure> {
        use frd::native_files::{ConflictPolicy, Directory, Limits};
        let defaults = Limits::default();
        let limits = Limits {
            max_file_bytes: self.max_file_bytes.unwrap_or(defaults.max_file_bytes),
            max_session_bytes: self.max_session_bytes.unwrap_or(defaults.max_session_bytes),
        };
        if limits.max_file_bytes == 0 || limits.max_session_bytes < limits.max_file_bytes {
            return Err(invalid_limits());
        }
        Directory::open(&self.directory, limits)
            .map(|directory| {
                directory.with_conflict_policy(if self.keep_both {
                    ConflictPolicy::KeepBoth
                } else {
                    ConflictPolicy::Reject
                })
            })
            .map_err(|reason| {
                Failure::new(
                    reason.code(),
                    "Choose an existing absolute --receive directory owned by you, not writable by other users or groups, with no symlink components. Nothing was received and no connection was made.",
                    2,
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{Command, Options};

    fn options(args: &str) -> Result<Options, Failure> {
        crate::options::parse(
            &args
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        )
    }

    fn connection(extra: &str) -> Result<crate::options::Connection, Failure> {
        let parsed = options(&format!(
            "connect n-host --control --experimental-native --display only {extra}"
        ))?;
        let Command::Connect(connection) = parsed.command else {
            unreachable!("connection required");
        };
        Ok(connection)
    }

    #[test]
    fn explicit_receive_policy_is_local_and_independent_of_other_capabilities() {
        let absent = connection("").unwrap();
        assert!(absent.receive.is_none());
        let default = connection("--receive /private/destination")
            .unwrap()
            .receive
            .unwrap();
        assert_eq!(default.max_file_bytes, None);
        assert_eq!(default.max_session_bytes, None);
        assert!(!default.keep_both);
        let configured = connection("--receive-conflict keep-both --receive-max-file-bytes 2147483648 --receive /private/destination --receive-max-session-bytes 4294967296").unwrap();
        assert!(!configured.clipboard && configured.audio.is_none() && configured.send.is_empty());
        let receive = configured.receive.unwrap();
        assert_eq!(receive.directory, PathBuf::from("/private/destination"));
        assert_eq!(receive.max_file_bytes, Some(2 * 1024 * 1024 * 1024));
        assert_eq!(receive.max_session_bytes, Some(4 * 1024 * 1024 * 1024));
        assert!(receive.keep_both);
        assert!(
            !connection("--receive /private/destination --receive-conflict reject")
                .unwrap()
                .receive
                .unwrap()
                .keep_both
        );
    }

    #[test]
    fn orphaned_unsafe_duplicate_and_overflowing_policy_refuses_without_echoing_values() {
        for flag in [
            "--receive-max-file-bytes 4",
            "--receive-max-session-bytes 8",
            "--receive-conflict keep-both",
        ] {
            assert_eq!(
                connection(flag).err().unwrap().code,
                "receive_directory_required"
            );
        }
        for value in ["0", "-1", "+4", "4GiB", "1.5", "18446744073709551616"] {
            for flag in ["--receive-max-file-bytes", "--receive-max-session-bytes"] {
                let failure = connection(&format!("--receive /private/destination {flag} {value}"))
                    .err()
                    .unwrap();
                assert_eq!(failure.code, "files_limits_invalid");
                assert!(!format!("{failure:?}").contains("/private/destination"));
            }
        }
        for extra in [
            "--receive-conflict overwrite",
            "--receive-conflict merge",
            "--receive-conflict resume",
            "--receive-conflict reject --receive-conflict keep-both",
            "--receive-max-file-bytes 4 --receive-max-file-bytes 8",
            "--receive-max-session-bytes 8 --receive-max-session-bytes 16",
            "--receive-max-file-bytes",
            "--receive-max-session-bytes",
            "--receive-conflict",
        ] {
            assert_eq!(
                connection(&format!("--receive /private/destination {extra}"))
                    .err()
                    .unwrap()
                    .code,
                "invalid_arguments"
            );
        }
        assert_eq!(connection("--receive /private/destination --receive-max-file-bytes 9 --receive-max-session-bytes 8").err().unwrap().code, "files_limits_invalid");
        for command in ["hosts", "doctor", "displays n-host --experimental-native"] {
            assert_eq!(
                options(&format!("{command} --receive-conflict keep-both"))
                    .err()
                    .unwrap()
                    .code,
                "invalid_arguments"
            );
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    fn effective_limits_refuse_before_path_access_including_programmatic_callers() {
        use frd::native_files::Limits;
        let defaults = Limits::default();
        for (file, session) in [
            (Some(0), None),
            (None, Some(0)),
            (Some(defaults.max_session_bytes + 1), None),
            (None, Some(defaults.max_file_bytes - 1)),
        ] {
            let request = ReceiveOptions {
                directory: PathBuf::from("relative-and-nonexistent"),
                max_file_bytes: file,
                max_session_bytes: session,
                keep_both: true,
            };
            assert_eq!(request.open().err().unwrap().code, "files_limits_invalid");
        }
    }

    #[test]
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    fn explicit_policy_reaches_the_pinned_directory_without_weakening_path_checks() {
        use frd::native_files::Limits;
        use std::{
            fs,
            os::unix::fs::{PermissionsExt, symlink},
            sync::atomic::{AtomicU64, Ordering},
        };
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "fr-download-options-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let private = root.join("private");
        fs::create_dir(&private).unwrap();
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        let request = connection(&format!(
            "--receive {} --receive-max-file-bytes 4294967296 --receive-max-session-bytes 8589934592 --receive-conflict keep-both",
            private.display(),
        )).unwrap().receive.unwrap();
        let directory = request.open().unwrap();
        assert_eq!(
            directory.limits(),
            Limits {
                max_file_bytes: 4 * 1024 * 1024 * 1024,
                max_session_bytes: 8 * 1024 * 1024 * 1024,
            }
        );
        let default = connection(&format!("--receive {}", private.display()))
            .unwrap()
            .receive
            .unwrap();
        assert_eq!(default.open().unwrap().limits(), Limits::default());
        let alias = root.join("alias");
        symlink(&private, &alias).unwrap();
        let unsafe_request = ReceiveOptions {
            directory: alias,
            ..request.clone()
        };
        assert_eq!(
            unsafe_request.open().err().unwrap().code,
            "files_directory_symlink"
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            request.open().err().unwrap().code,
            "files_directory_writable_by_others"
        );
        // Existing descriptors and their budgets remain pinned. Changing path
        // permissions cannot authorize a fresh receive owner through this API.
        assert_eq!(directory.limits().max_session_bytes, 8 * 1024 * 1024 * 1024);
        drop(directory);
        fs::remove_dir_all(root).unwrap();
    }
}
