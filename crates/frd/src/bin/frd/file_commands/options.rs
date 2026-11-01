//! Strict local command parsing. It performs no filesystem or socket access.
use std::path::{Component, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Action {
    Send(Vec<PathBuf>),
    Status(Option<u64>),
    Cancel(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Options {
    pub target: String,
    pub instance: Option<String>,
    pub action: Action,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Invalid;

impl Options {
    pub fn parse(args: &[String]) -> Result<Self, Invalid> {
        let mut target = None;
        let mut instance = None;
        let mut status = false;
        let mut request = None;
        let mut cancel = None;
        let mut json = false;
        let mut paths = Vec::new();
        let mut path_bytes = 0_usize;
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--json" if !json => json = true,
                "--status" if !status => status = true,
                "--target" if target.is_none() => {
                    target = Some(iter.next().ok_or(Invalid)?.clone());
                }
                "--instance" if instance.is_none() => {
                    instance = Some(iter.next().ok_or(Invalid)?.clone());
                }
                "--request" if request.is_none() => request = Some(number(iter.next())?),
                "--cancel" if cancel.is_none() => cancel = Some(number(iter.next())?),
                _ if arg.starts_with('-') => return Err(Invalid),
                _ => {
                    let path = PathBuf::from(arg);
                    if paths.len() == 8
                        || arg.len() > 4096
                        || arg.as_bytes().contains(&0)
                        || !path.is_absolute()
                        || path.components().any(|part| part == Component::ParentDir)
                    {
                        return Err(Invalid);
                    }
                    path_bytes = path_bytes.checked_add(arg.len()).ok_or(Invalid)?;
                    if path_bytes > 32768 {
                        return Err(Invalid);
                    }
                    paths.push(path);
                }
            }
        }
        let target = target.ok_or(Invalid)?;
        if target.is_empty()
            || target.len() > 64
            || !target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
            || instance.as_ref().is_some_and(|value| {
                value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Err(Invalid);
        }
        // An old sequence number is meaningful only with its original owner.
        // Never inspect a replacement and apply a former command's cancellation.
        if (request.is_some() || cancel.is_some()) && instance.is_none() {
            return Err(Invalid);
        }
        let action = if status {
            if cancel.is_some() || !paths.is_empty() {
                return Err(Invalid);
            }
            Action::Status(request)
        } else if let Some(sequence) = cancel {
            if request.is_some() || !paths.is_empty() {
                return Err(Invalid);
            }
            Action::Cancel(sequence)
        } else {
            if request.is_some() || paths.is_empty() {
                return Err(Invalid);
            }
            Action::Send(paths)
        };
        Ok(Self {
            target,
            instance,
            action,
        })
    }
}

fn number(value: Option<&String>) -> Result<u64, Invalid> {
    let value = value.ok_or(Invalid)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Invalid);
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value != 0)
        .ok_or(Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    const INSTANCE: &str = "0123456789abcdef0123456789abcdef";
    fn parse(args: &[&str]) -> Result<Options, Invalid> {
        Options::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }
    #[test]
    fn new_send_and_current_status_are_explicit_and_bounded() {
        assert_eq!(
            parse(&[
                "--target",
                "workstation",
                "/home/me/report",
                "/home/me/tree",
                "--json"
            ])
            .unwrap(),
            Options {
                target: "workstation".into(),
                instance: None,
                action: Action::Send(vec!["/home/me/report".into(), "/home/me/tree".into()]),
            }
        );
        assert_eq!(
            parse(&["--target", "workstation", "--status"])
                .unwrap()
                .action,
            Action::Status(None)
        );
        let mut paths = vec!["--target", "workstation"];
        paths.extend(["/file"; 8]);
        assert!(parse(&paths).is_ok());
        paths.push("/ninth");
        assert!(parse(&paths).is_err());
    }
    #[test]
    fn history_and_cancellation_require_the_original_instance() {
        for args in [
            vec!["--target", "host", "--status", "--request", "1"],
            vec!["--target", "host", "--cancel", "1"],
            vec!["--target", "host", "--instance", INSTANCE, "--request", "1"],
            vec!["--target", "host", "--instance", INSTANCE, "--cancel", "0"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert_eq!(
            parse(&[
                "--target",
                "host",
                "--instance",
                INSTANCE,
                "--status",
                "--request",
                "2"
            ])
            .unwrap()
            .action,
            Action::Status(Some(2))
        );
        assert_eq!(
            parse(&["--target", "host", "--instance", INSTANCE, "--cancel", "1"])
                .unwrap()
                .action,
            Action::Cancel(1)
        );
    }
    #[test]
    fn ambiguous_commands_and_unsafe_path_spellings_refuse_before_io() {
        for args in [
            vec![],
            vec!["/file"],
            vec!["--target", "host"],
            vec!["--target", "host", "--status", "/file"],
            vec!["--target", "host", "--status", "--status"],
            vec!["--target", "host", "--status", "--json", "--json"],
            vec!["--target", "host", "--target", "other", "/file"],
            vec!["--target", "host", "--instance", "wrong", "/file"],
            vec!["--target", "host", "--unknown", "/file"],
            vec!["--target", "host", "relative"],
            vec!["--target", "host", "/home/../private"],
            vec!["--target", "host", "/file\0hidden"],
            vec!["--target", "host/other", "/file"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
        assert!(parse(&["--target", &"n".repeat(65), "/file"]).is_err());
        assert!(parse(&["--target", "host", &format!("/{}", "x".repeat(4096))]).is_err());
    }
}
