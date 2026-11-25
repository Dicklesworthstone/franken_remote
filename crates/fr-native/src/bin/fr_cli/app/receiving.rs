//! One explicit native-app receiving choice, consumed before its viewer starts.
//!
//! The existing connect parser owns path, count, limit and conflict validation.
//! This shell retains only its validated local arguments. The original viewer
//! opens and pins the destination before any remote connection, and its own
//! session owner fences receiving on control loss or replacement.
use super::{Failure, notice_text};
use crate::options::{self, Command};
use fr_wire::sync_job_state::JobMode;
use std::fmt;

/// Deliberately neither `Copy` nor `Clone`: one app launch may authorize receiving
/// on at most one newly selected controlling connection.
#[derive(PartialEq, Eq)]
pub(super) struct Receiving {
    arguments: Vec<String>,
    sync_job: Option<(u64, JobMode)>,
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
        let receive = connection.receive.ok_or_else(invalid)?;
        let sync_job = receive.sync_job;
        #[cfg(any(
            all(target_os = "linux", feature = "linux-desktop"),
            all(target_os = "macos", feature = "macos-desktop")
        ))]
        receive.effective_limits()?;
        #[cfg(not(any(
            all(target_os = "linux", feature = "linux-desktop"),
            all(target_os = "macos", feature = "macos-desktop")
        )))]
        let _ = receive;
        Ok(Some(Self {
            arguments,
            sync_job,
        }))
    }

    pub(super) fn sync_job(&self) -> Option<u64> {
        self.sync_job.map(|(id, _)| id)
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

/// Native Linux and macOS acquire a fresh destination in the selected viewer.
/// A missing desktop portal is a typed pre-start refusal, not permission to
/// reuse another connection's destination.
pub(super) fn directory_picker_available() -> bool {
    cfg!(any(
        all(target_os = "linux", feature = "linux-desktop"),
        all(target_os = "macos", feature = "macos-desktop")
    ))
}

pub(super) fn available(pending: Option<&Receiving>) -> bool {
    pending.is_some() || directory_picker_available()
}

/// Consume a configured destination exactly once. Without one, fresh picker
/// consent requests a directory chooser inside this viewer only; no pathname
/// or receiving permission is stored in the application preferences.
pub(super) fn take_for_app_choice(
    pending: &mut Option<Receiving>,
    mut arguments: Vec<String>,
    consent: bool,
) -> Result<Vec<String>, Failure> {
    if !consent || pending.is_some() {
        return take_for_choice(pending, arguments, consent);
    }
    if !directory_picker_available() {
        return Err(invalid());
    }
    arguments.push("--choose-receive-directory".to_owned());
    options::parse(&arguments)?;
    Ok(arguments)
}

/// The pending choice remains visible even after an unused picker refresh.
/// Neither the destination nor any transfer contents enter this UI notice.
pub(super) fn notice(pending: Option<&Receiving>, previous: Option<&str>) -> Option<String> {
    pending.map(|receiving| {
        let scope = receiving.sync_job.map_or_else(
            || "Receive host files once: choose View and control, then Receive files. The destination applies only to the next connection.".to_owned(),
            |(job, mode)| {
                let scope = if mode == JobMode::Recursive {
                    " including nested directories"
                } else {
                    ""
                };
                format!(
                    "Accept host folder job {job}{scope}: choose View and control, then Receive files. New source versions are received throughout this connection, keeping both on conflicts. This job and destination apply only to the next connection."
                )
            },
        );
        notice_text(&format!("{scope} {}", previous.unwrap_or_default()))
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

/// The picker can explicitly decline this one-use destination. Consume it
/// before either choice launches a viewer; neither an unchecked choice nor a
/// failed launch leaves receiving available on a later connection.
pub(super) fn take_for_choice(
    pending: &mut Option<Receiving>,
    arguments: Vec<String>,
    consent: bool,
) -> Result<Vec<String>, Failure> {
    if consent {
        if pending.is_none() {
            return Err(invalid());
        }
        take_for_session(pending, arguments)
    } else {
        let _ = pending.take();
        Ok(arguments)
    }
}
