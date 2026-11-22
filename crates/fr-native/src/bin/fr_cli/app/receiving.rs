//! One explicit native-app receiving choice, consumed before its viewer starts.
//!
//! The existing connect parser owns path, count, limit and conflict validation.
//! This shell retains only its validated local arguments. The original viewer
//! opens and pins the destination before any remote connection, and its own
//! session owner fences receiving on control loss or replacement.
use super::{Failure, notice_text};
use crate::options::{self, Command};
use std::fmt;

/// Deliberately neither `Copy` nor `Clone`: one app launch may authorize receiving
/// on at most one newly selected controlling connection.
#[derive(PartialEq, Eq)]
pub(super) struct Receiving {
    arguments: Vec<String>,
}

impl fmt::Debug for Receiving {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Receiving").finish_non_exhaustive()
    }
}

impl Receiving {
    pub(super) fn parse(arguments: Vec<String>) -> Result<Option<Self>, Failure> {
        if arguments.is_empty() {
            return Ok(None);
        }
        // Parsing this ordinary choice opens no picker, directory or session.
        // It avoids a second implementation of the receive policy in the app.
        let mut validation: Vec<String> = [
            "connect",
            "--choose-host",
            "--control",
            "--experimental-native",
            "--display",
            "only",
        ]
        .map(str::to_owned)
        .into();
        validation.extend(arguments.iter().cloned());
        let Command::Connect(connection) = options::parse(&validation)?.command else {
            return Err(invalid());
        };
        if connection.receive.is_none() {
            return Err(invalid());
        }
        Ok(Some(Self { arguments }))
    }

    fn into_session_args(self, mut arguments: Vec<String>) -> Result<Vec<String>, Failure> {
        arguments.extend(self.arguments);
        // Revalidate the actual selected role and complete child invocation.
        // In particular, a view-only pick cannot turn the prior local receive
        // choice into control or silently discard it and start a viewer.
        options::parse(&arguments)?;
        Ok(arguments)
    }
}

fn invalid() -> Failure {
    Failure::new(
        "invalid_arguments",
        "Run fr app --help. Receiving requires an explicit local destination.",
        2,
    )
}

/// The pending choice remains visible even after an unused picker refresh.
/// Neither the destination nor any transfer contents enter this UI notice.
pub(super) fn notice(pending: Option<&Receiving>, previous: Option<&str>) -> Option<String> {
    pending.map(|_| {
        notice_text(&format!(
            "Receive host files once: choose View and control. The destination applies only to the next connection. {}",
            previous.unwrap_or_default()
        ))
    })
}

/// Take before launch, including when actual command validation refuses.
/// Subsequent picks cannot regain this local choice from preferences or a
/// previous report. Cancellation drops the containing application owner.
pub(super) fn take_for_session(
    pending: &mut Option<Receiving>,
    arguments: Vec<String>,
) -> Result<Vec<String>, Failure> {
    match pending.take() {
        Some(receiving) => receiving.into_session_args(arguments),
        None => Ok(arguments),
    }
}
