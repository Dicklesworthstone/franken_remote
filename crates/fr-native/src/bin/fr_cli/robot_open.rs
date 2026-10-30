//! Explicit foreground robot sessions use the canonical native connection owner.
//!
//! This is not a detached session broker or an input-lease mint. The process
//! remains alive until the original connection and its native cleanup finish;
//! local session status/close address that same owner. Keeping the translation
//! at the CLI boundary avoids a second dialer, consent path or reconnect policy.
use super::{Failure, options};

fn usage() -> Failure {
    Failure::new(
        "invalid_arguments",
        "Use fr robot session open TARGET --foreground --role view|control followed by the explicit native connection options; run fr --help.",
        2,
    )
}

pub(super) fn parse(args: &[String]) -> Result<options::Options, Failure> {
    let opening = args.get(0..3).is_some_and(|prefix| {
        prefix[0] == "robot" && prefix[1] == "session" && prefix[2] == "open"
    });
    if !opening || !args.iter().skip(4).any(|arg| arg == "--foreground") {
        // Preserve the reserved broker grammar, including its typed runtime
        // refusal. Old scripts do not silently become long-running processes.
        return options::parse(args);
    }
    // Bound the ORIGINAL request, not the shorter translated argument vector.
    if args.len() > 32
        || args
            .iter()
            .any(|arg| arg.len() > 4096 || arg.chars().any(char::is_control))
    {
        return Err(usage());
    }
    let node = args.get(3).ok_or_else(usage)?;
    // A deliberate prefix makes the lifetime contract unambiguous and avoids
    // stripping an option-looking value out of another flag's argument slot.
    if args.get(4).map(String::as_str) != Some("--foreground")
        || args.get(5).map(String::as_str) != Some("--role")
    {
        return Err(usage());
    }
    let role = match args.get(6).map(String::as_str) {
        Some("view") => "--view-only",
        Some("control") => "--control",
        _ => return Err(usage()),
    };
    let mut connect = Vec::with_capacity(args.len() - 4);
    connect.extend(["connect".to_owned(), node.clone(), role.to_owned()]);
    connect.extend_from_slice(&args[7..]);
    // Exactly the same parser checks trust paths, role-only capabilities,
    // display policy, transport opt-in, limits and duplicate flags. It also
    // preserves --socket rather than quietly using the installed default.
    // The resulting Connect command takes the ordinary live platform path.
    options::parse(&connect)
}

#[cfg(test)]
#[path = "robot_open/tests.rs"]
mod tests;
