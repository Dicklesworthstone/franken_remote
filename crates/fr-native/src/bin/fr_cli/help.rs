//! Bounded, static help before any runtime, local endpoint or native operation.
//! Only exact command paths are accepted: a help flag does not conceal extra
//! arguments or accidentally become an operational command's option value.
#![forbid(unsafe_code)]

pub(super) const OVERVIEW: &str = include_str!("help.txt");
const ROBOT: &str = include_str!("help/robot.txt");
const OBSERVE: &str = include_str!("robot_observe/help.txt");
const INPUT: &str = include_str!("robot_input/help.txt");
const SEND: &str = include_str!("file_commands/help.txt");

#[derive(Debug, PartialEq, Eq)]
pub(super) struct InvalidRequest;

/// The caller bounds and converts the original OS arguments before dispatch.
/// `None` means an ordinary command; an invalid help request must not run it.
pub(super) fn page(args: &[String]) -> Result<Option<&'static str>, InvalidRequest> {
    let topic = match args {
        [] => return Ok(Some(OVERVIEW)),
        [help, flag] if help == "help" && flag == "--help" => return Ok(Some(OVERVIEW)),
        [help, topic @ ..] if help == "help" => topic,
        [topic @ .., flag] if flag == "--help" => topic,
        _ if args.iter().any(|arg| arg == "--help") => return Err(InvalidRequest),
        _ => return Ok(None),
    };
    let text = match topic {
        [] => OVERVIEW,
        [command]
            if matches!(
                command.as_str(),
                "connect"
                    | "hosts"
                    | "peers"
                    | "status"
                    | "doctor"
                    | "inspect"
                    | "displays"
                    | "disconnect"
            ) =>
        {
            OVERVIEW
        }
        [command] if command == "robot" => ROBOT,
        [command] if command == "send" => SEND,
        [command, action]
            if command == "hosts" && matches!(action.as_str(), "save" | "saved" | "forget") =>
        {
            OVERVIEW
        }
        [command, topic] if command == "robot" => match topic.as_str() {
            "session" => OVERVIEW,
            "observe" => OBSERVE,
            "input" => INPUT,
            _ => return Err(InvalidRequest),
        },
        [command, topic, action]
            if command == "robot"
                && topic == "session"
                && matches!(action.as_str(), "open" | "status" | "close") =>
        {
            OVERVIEW
        }
        _ => return Err(InvalidRequest),
    };
    Ok(Some(text))
}

#[cfg(test)]
#[path = "help/tests.rs"]
mod tests;
