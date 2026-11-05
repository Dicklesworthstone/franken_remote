//! No filesystem, socket or authority access while parsing a local command.
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
    pub connection: String,
    pub lease: Option<String>,
    pub action: Action,
    pub json: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Invalid;

impl Options {
    pub fn parse(args: &[String]) -> Result<Self, Invalid> {
        if args.len() > 32 || args.iter().any(|arg| arg.len() > 4096) {
            return Err(Invalid);
        }
        let mut iter = args.iter();
        if iter.next().is_none_or(|arg| arg != "send") {
            return Err(Invalid);
        }
        let target = iter.next().ok_or(Invalid)?.clone();
        if target.is_empty()
            || target.len() > 253
            || target.starts_with('-')
            || target.chars().any(char::is_control)
        {
            return Err(Invalid);
        }
        let (mut connection, mut lease) = (None, None);
        let (mut status, mut json) = (false, false);
        let (mut request, mut cancel) = (None, None);
        let mut paths = Vec::new();
        let mut path_bytes = 0_usize;
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--json" if !json => json = true,
                "--status" if !status => status = true,
                "--connection-id" if connection.is_none() => {
                    connection = Some(handle(iter.next())?);
                }
                "--lease" if lease.is_none() => lease = Some(handle(iter.next())?),
                "--request" if request.is_none() => request = Some(number(iter.next())?),
                "--cancel" if cancel.is_none() => cancel = Some(number(iter.next())?),
                "--send" => {
                    let value = iter.next().ok_or(Invalid)?;
                    let path = PathBuf::from(value);
                    if paths.len() == 8
                        || value.as_bytes().contains(&0)
                        || !path.is_absolute()
                        || path.components().any(|part| part == Component::ParentDir)
                    {
                        return Err(Invalid);
                    }
                    path_bytes = path_bytes.checked_add(value.len()).ok_or(Invalid)?;
                    if path_bytes > 32768 {
                        return Err(Invalid);
                    }
                    paths.push(path);
                }
                _ => return Err(Invalid),
            }
        }
        let connection = connection.ok_or(Invalid)?;
        let action = if status && cancel.is_none() && paths.is_empty() {
            Action::Status(request)
        } else if let Some(sequence) =
            cancel.filter(|_| !status && request.is_none() && paths.is_empty())
        {
            Action::Cancel(sequence)
        } else if !status && request.is_none() && cancel.is_none() && !paths.is_empty() {
            Action::Send(paths)
        } else {
            return Err(Invalid);
        };
        // Only a read of the latest status may discover an instance. Never
        // discover a replacement and apply an old request or a new selection.
        if action != Action::Status(None) && lease.is_none() {
            return Err(Invalid);
        }
        Ok(Self {
            target,
            connection,
            lease,
            action,
            json,
        })
    }
}
fn handle(value: Option<&String>) -> Result<String, Invalid> {
    let value = value.ok_or(Invalid)?;
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(Invalid);
    }
    Ok(value.to_ascii_lowercase())
}
fn number(value: Option<&String>) -> Result<u64, Invalid> {
    let value = value.ok_or(Invalid)?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(Invalid);
    }
    value
        .parse::<u64>()
        .ok()
        .filter(|number| *number != 0)
        .ok_or(Invalid)
}

#[cfg(test)]
#[path = "options/tests.rs"]
mod tests;
