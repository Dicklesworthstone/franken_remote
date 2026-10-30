//! Explicit, finite native observation; not a query of an existing controller.
//! Keep the reserved broker grammar unchanged unless --native-once is present.
use super::{Failure, options};
use std::{collections::BTreeSet, path::PathBuf};

#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
#[path = "robot_observe/linux.rs"]
mod linux;
#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
#[path = "robot_observe/report.rs"]
mod report;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Minimum {
    Decoded,
    SubmittedToCompositor,
}

#[cfg_attr(
    not(all(target_os = "linux", feature = "linux-desktop")),
    allow(dead_code)
)]
struct Request {
    connection: options::Connection,
    socket: Option<PathBuf>,
    json: bool,
    minimum: Minimum,
}

pub(super) fn requested(args: &[String]) -> bool {
    args.first().is_some_and(|arg| arg == "robot")
        && args.get(1).is_some_and(|arg| arg == "observe")
        && args.iter().skip(3).any(|arg| arg == "--native-once")
}

fn usage() -> Failure {
    Failure::new(
        "invalid_arguments",
        "Use fr robot observe TARGET --native-once --experimental-native --display HANDLE|only with read-only native options; run fr --help.",
        2,
    )
}

fn parse(args: &[String]) -> Result<Request, Failure> {
    // Check the original vector before translating away the command prefix.
    if args.len() > 32
        || args
            .iter()
            .any(|arg| arg.len() > 4096 || arg.chars().any(char::is_control))
        || !requested(args)
        || args.get(3).map(String::as_str) != Some("--native-once")
    {
        return Err(usage());
    }
    let node = args.get(2).ok_or_else(usage)?;
    let mut connect = vec![
        "connect".to_owned(),
        node.clone(),
        "--view-only".to_owned(),
        "--attempts".to_owned(),
        "1".to_owned(),
    ];
    let mut minimum = Minimum::Decoded;
    let mut index = 4;
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) {
            return Err(usage());
        }
        match flag {
            "--experimental-native" | "--by-name" | "--ipv6" | "--json" => {
                connect.push(flag.to_owned());
            }
            "--display" | "--worker" | "--trust-roots" | "--x-display" | "--socket"
            | "--port" | "--fit" | "--evidence-level" => {
                let value = args
                    .get(index)
                    .filter(|value| !value.is_empty() && !value.starts_with("--"))
                    .ok_or_else(usage)?;
                index += 1;
                if flag == "--evidence-level" {
                    minimum = match value.as_str() {
                        "decoded" => Minimum::Decoded,
                        "submitted_to_compositor" => Minimum::SubmittedToCompositor,
                        "instrumentally_observed" => {
                            return Err(Failure::new(
                                "observation_evidence_unavailable",
                                "This native path reports decoder or compositor submission receipts, not instrumentally observed physical visibility.",
                                2,
                            ));
                        }
                        _ => return Err(usage()),
                    };
                } else {
                    connect.extend([flag.to_owned(), value.clone()]);
                }
            }
            "--screenshot" => {
                return Err(Failure::new(
                    "observation_artifact_unavailable",
                    "One-shot native observation currently returns metadata only. No screenshot file was opened or written.",
                    2,
                ));
            }
            _ => return Err(usage()),
        }
    }
    // Reuse all canonical identity, trust, display, role and resource checks.
    // The whitelist cannot offer auxiliary capabilities or request a retry.
    let options = options::parse(&connect)?;
    let options::Command::Connect(connection) = options.command else {
        return Err(usage());
    };
    if connection.display == options::DisplayChoice::Choose {
        return Err(Failure::new(
            "observation_display_required",
            "A one-shot observation needs a specific display handle or --display only; it does not open an interactive display chooser.",
            2,
        ));
    }
    Ok(Request {
        connection,
        socket: options.socket,
        json: options.json,
        minimum,
    })
}

pub(super) fn run(args: &[String]) -> Result<String, Failure> {
    let request = parse(args)?;
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    {
        linux::run(&request)
    }
    #[cfg(not(all(target_os = "linux", feature = "linux-desktop")))]
    {
        let _ = request;
        Err(Failure::new(
            if cfg!(target_os = "linux") {
                "gui_unavailable"
            } else {
                "platform_unavailable"
            },
            "One-shot native observation requires the Linux desktop build and a local X11 display; it is not a headless screenshot command.",
            2,
        ))
    }
}

#[cfg(test)]
#[path = "robot_observe/tests.rs"]
mod tests;
