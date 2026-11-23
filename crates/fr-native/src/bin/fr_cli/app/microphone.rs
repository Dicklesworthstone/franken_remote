//! An explicit source belongs to one app choice, not the saved preferences.
//! The picker sees availability only; the selected viewer pins and supervises
//! the ordinary microphone source, starting muted with fresh Allow/Talk gates.
use super::Failure;
use crate::options::{self, Command};
use std::fmt;

#[derive(PartialEq, Eq)]
pub(super) struct Microphone {
    arguments: Vec<String>,
}

impl fmt::Debug for Microphone {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Microphone").finish_non_exhaustive()
    }
}

impl Microphone {
    pub(super) fn parse(arguments: Vec<String>) -> Result<Option<Self>, Failure> {
        if arguments.is_empty() {
            return Ok(None);
        }
        // Device syntax and role rules have one owner. Parsing opens no device,
        // worker, picker or connection and does not resolve a default source.
        let mut validation: Vec<String> = [
            "connect",
            "--choose-host",
            "--control",
            "--experimental-native",
            "--display",
            "only",
            "--microphone",
        ]
        .map(str::to_owned)
        .into();
        validation.extend(arguments.iter().cloned());
        let Command::Connect(connection) = options::parse(&validation)?.command else {
            return Err(unavailable());
        };
        if connection.microphone.is_none() || !build_available() {
            return Err(unavailable());
        }
        Ok(Some(Self { arguments }))
    }
}

fn build_available() -> bool {
    cfg!(any(
        all(target_os = "linux", feature = "linux-microphone"),
        all(target_os = "macos", feature = "macos-microphone")
    ))
}

pub(super) fn available(pending: Option<&Microphone>) -> bool {
    build_available() && (pending.is_some() || cfg!(target_os = "macos"))
}

/// Take before validating or launching, even for a view-only choice or a
/// decline. Failure cannot transfer this source to another machine/session.
pub(super) fn take_for_choice(
    pending: &mut Option<Microphone>,
    mut arguments: Vec<String>,
    consent: bool,
) -> Result<Vec<String>, Failure> {
    let source = pending.take();
    if !consent {
        return Ok(arguments);
    }
    if !available(source.as_ref()) {
        return Err(unavailable());
    }
    arguments.push("--microphone".to_owned());
    if let Some(source) = source {
        arguments.extend(source.arguments);
    } else {
        // Only the macOS build can reach this path; its existing native owner
        // pins the current CoreAudio input once before network startup.
        arguments.extend(["--microphone-source", "default"].map(str::to_owned));
    }
    options::parse(&arguments)?;
    Ok(arguments)
}

fn unavailable() -> Failure {
    Failure::new(
        "app_microphone_unavailable",
        "Microphone controls require a local microphone build. On Linux, start fr app with --microphone-source NAME for the next connection. No connection was opened.",
        2,
    )
}

#[cfg(test)]
#[path = "microphone_tests.rs"]
mod tests;
