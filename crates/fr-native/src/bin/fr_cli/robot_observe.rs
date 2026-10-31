//! Explicit native observations and authenticated reads of an opt-in live owner.
use super::{Failure, options};
use std::{collections::BTreeSet, path::PathBuf};

#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
#[path = "robot_observe/linux.rs"]
mod linux;
#[path = "robot_observe/local.rs"]
mod local;
#[cfg(all(target_os = "linux", feature = "linux-desktop"))]
#[path = "robot_observe/report.rs"]
mod report;
#[cfg(any(target_os = "linux", test))]
#[path = "robot_observe/artifact.rs"]
mod artifact;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Minimum {
    Decoded,
    SubmittedToCompositor,
}
#[cfg_attr(not(all(target_os = "linux", feature = "linux-desktop")), allow(dead_code))]
struct Request {
    connection: options::Connection,
    socket: Option<PathBuf>,
    json: bool,
    minimum: Minimum,
    live: bool,
    screenshot: Option<PathBuf>,
}
fn observing(args: &[String]) -> bool {
    args.first().is_some_and(|arg| arg == "robot")
        && args.get(1).is_some_and(|arg| arg == "observe")
}
fn live_requested(args: &[String]) -> bool {
    args.get(0..3).is_some_and(|prefix| prefix[0] == "robot" && prefix[1] == "session" && prefix[2] == "open")
        && args.iter().skip(4).any(|arg| arg == "--publish-observations")
}
pub(super) fn requested(args: &[String]) -> bool {
    (observing(args) && args.iter().skip(3).any(|arg| arg == "--native-once" || arg == "--connection-id"))
        || live_requested(args)
}
fn usage() -> Failure {
    Failure::new(
        "invalid_arguments",
        "Use fr robot observe TARGET --native-once --experimental-native --display HANDLE|only, or --connection-id ID for an existing observation publisher; run fr --help.",
        2,
    )
}
fn bounded_args(args: &[String]) -> Result<(), Failure> {
    if args.len() > 32 || args.iter().any(|arg| arg.len() > 4096 || arg.chars().any(char::is_control)) {
        return Err(usage());
    }
    Ok(())
}
fn parse(args: &[String]) -> Result<Request, Failure> {
    bounded_args(args)?;
    if !observing(args) || args.get(3).map(String::as_str) != Some("--native-once") {
        return Err(usage());
    }
    let node = args.get(2).ok_or_else(usage)?;
    let mut connect = vec!["connect".to_owned(), node.clone(), "--view-only".to_owned(), "--attempts".to_owned(), "1".to_owned()];
    let mut minimum = Minimum::Decoded;
    let mut screenshot = None;
    let mut index = 4;
    let mut seen = BTreeSet::new();
    while index < args.len() {
        let flag = args[index].as_str();
        index += 1;
        if !seen.insert(flag) { return Err(usage()); }
        match flag {
            "--experimental-native" | "--by-name" | "--ipv6" | "--json" => connect.push(flag.to_owned()),
            "--display" | "--worker" | "--trust-roots" | "--x-display" | "--socket" | "--port" | "--fit" | "--evidence-level" => {
                let value = args.get(index).filter(|value| !value.is_empty() && !value.starts_with("--")).ok_or_else(usage)?;
                index += 1;
                if flag == "--evidence-level" {
                    minimum = match value.as_str() {
                        "decoded" => Minimum::Decoded,
                        "submitted_to_compositor" => Minimum::SubmittedToCompositor,
                        "instrumentally_observed" => return Err(Failure::new(
                            "observation_evidence_unavailable",
                            "Native receipts prove decoding or compositor submission, not instrumentally observed physical visibility.", 2)),
                        _ => return Err(usage()),
                    };
                } else {
                    connect.extend([flag.to_owned(), value.clone()]);
                }
            }
            "--screenshot" => {
                let value = args.get(index).filter(|value| !value.is_empty() && !value.starts_with("--")).ok_or_else(usage)?;
                index += 1;
                screenshot = Some(PathBuf::from(value));
            }
            _ => return Err(usage()),
        }
    }
    let options = options::parse(&connect)?;
    let options::Command::Connect(connection) = options.command else { return Err(usage()); };
    if connection.display == options::DisplayChoice::Choose {
        return Err(Failure::new(
            "observation_display_required",
            "Observation needs a specific display handle or --display only; it does not open an interactive display chooser.", 2));
    }
    Ok(Request { connection, socket: options.socket, json: options.json, minimum, live: false, screenshot })
}
fn parse_live(args: &[String]) -> Result<Request, Failure> {
    bounded_args(args)?;
    if !live_requested(args)
        || args.get(4).map(String::as_str) != Some("--foreground")
        || args.get(5).map(String::as_str) != Some("--role")
        || args.get(6).map(String::as_str) != Some("view")
        || args.iter().filter(|arg| *arg == "--publish-observations").count() != 1
    { return Err(usage()); }
    let mut translated = vec!["robot".into(), "observe".into(), args[3].clone(), "--native-once".into()];
    translated.extend(args[7..].iter().filter(|arg| *arg != "--publish-observations").cloned());
    let mut request = parse(&translated)?;
    if request.screenshot.is_some() {
        return Err(Failure::new(
            "observation_artifact_unavailable",
            "Live publication returns metadata only. Use an explicit --native-once observation to request a screenshot; no file was opened.", 2));
    }
    request.live = true;
    Ok(request)
}
pub(super) fn run(args: &[String]) -> Result<String, Failure> {
    if observing(args) && args.iter().skip(3).any(|arg| arg == "--connection-id") {
        return local::run(args);
    }
    let request = if live_requested(args) { parse_live(args)? } else { parse(args)? };
    #[cfg(all(target_os = "linux", feature = "linux-desktop"))]
    { linux::run(&request) }
    #[cfg(not(all(target_os = "linux", feature = "linux-desktop")))]
    {
        let _ = request;
        Err(Failure::new(
            if cfg!(target_os = "linux") { "gui_unavailable" } else { "platform_unavailable" },
            "Native observation requires the Linux desktop build and a local X11 display; it is not a headless screenshot command.", 2))
    }
}

#[cfg(test)]
#[path = "robot_observe/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "robot_observe/live_tests.rs"]
mod live_tests;
