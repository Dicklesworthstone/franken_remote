#![forbid(unsafe_code)]
// Off Linux every command refuses before using the shared option/output code.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
//! Native CLI entry point. Hosting is never enabled by installing/running it.
#[path = "fr_cli/closure.rs"]
mod closure;
#[path = "fr_cli/file_commands.rs"]
mod file_commands;
#[path = "fr_cli/help.rs"]
mod help;
#[cfg(target_os = "linux")]
#[path = "fr_cli/linux.rs"]
mod linux;
#[path = "fr_cli/options.rs"]
mod options;
#[path = "fr_cli/output.rs"]
mod output;
#[path = "fr_cli/robot_input.rs"]
mod robot_input;
#[path = "fr_cli/robot_observe.rs"]
mod robot_observe;
#[path = "fr_cli/robot_open.rs"]
mod robot_open;
#[cfg(target_os = "linux")]
#[path = "fr_cli/saved_hosts.rs"]
mod saved_hosts;
#[cfg(target_os = "linux")]
#[path = "fr_cli/session_control.rs"]
mod session_control;
#[path = "fr_cli/terminal.rs"]
mod terminal;
use std::process::ExitCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Failure {
    code: &'static str,
    next: &'static str,
    exit: u8,
    /// The command may already have changed its original owner, but no result
    /// was collected. A failed local reply must not be presented as refusal.
    unknown_effect: bool,
    revocation: Option<terminal::Report>,
    closure: Option<closure::Report>,
    /// Earlier attempts' typed ends, when a reconnect's own end would hide them.
    earlier: output::Earlier,
    /// Bounded receive evidence retained through every terminal error formatter.
    files_receive: Option<&'static FileReceiveReport>,
    /// Original control/transfer outcomes survive a later viewing retry's end.
    control_history: Option<&'static ControlHistory>,
}
#[derive(Debug, PartialEq, Eq)]
struct FileReceiveReport {
    json: String,
    text: String,
}
#[derive(Debug, PartialEq, Eq)]
struct ControlHistory {
    json: String,
    text: String,
}
impl Failure {
    const fn new(code: &'static str, next: &'static str, exit: u8) -> Self {
        Self {
            code,
            next,
            exit,
            unknown_effect: false,
            revocation: None,
            closure: None,
            earlier: output::Earlier::NONE,
            files_receive: None,
            control_history: None,
        }
    }
    fn uncertain(mut self) -> Self {
        self.unknown_effect = true;
        self
    }
}
fn main() -> ExitCode {
    let mut args = Vec::new();
    let mut json = false;
    for arg in std::env::args_os().skip(1) {
        if arg == "--json" {
            json = true;
        }
        if args.len() == 32 || arg.len() > 4096 {
            return finish(
                Err(Failure::new(
                    "invalid_arguments",
                    "Run fr --help; argument limits exceeded.",
                    2,
                )),
                json,
            );
        }
        let Ok(arg) = arg.into_string() else {
            return finish(
                Err(Failure::new("invalid_arguments", "Use UTF-8 arguments.", 2)),
                json,
            );
        };
        args.push(arg);
    }
    // Help is local and platform-independent, but never bypasses the original
    // argument limits above or turns a malformed command into a successful one.
    match help::page(&args) {
        Ok(Some(text)) => {
            return if output::write(text).is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(74)
            };
        }
        Err(_) => {
            return finish(
                Err(Failure::new(
                    "invalid_arguments",
                    "Use fr help TOPIC or fr COMMAND --help; run fr --help to list topics.",
                    2,
                )),
                json,
            );
        }
        Ok(None) => {}
    }
    if file_commands::requested(&args) {
        return match file_commands::run(&args) {
            Ok(reply) => {
                if output::write(&reply.text).is_ok() {
                    ExitCode::from(reply.exit)
                } else {
                    ExitCode::from(74)
                }
            }
            Err(failure) => finish(Err(failure), json),
        };
    }
    if robot_input::requested(&args) {
        return match robot_input::run(&args) {
            Ok(reply) => {
                if output::write(&reply.text).is_ok() {
                    ExitCode::from(reply.exit)
                } else {
                    ExitCode::from(74)
                }
            }
            Err(failure) => finish(Err(failure), json),
        };
    }
    if robot_observe::requested(&args) {
        return finish(robot_observe::run(&args), json);
    }
    let parsed = match robot_open::parse(&args) {
        Ok(v) => v,
        Err(e) => return finish(Err(e), json),
    };
    if matches!(parsed.command, options::Command::Help) {
        return if output::write(help::OVERVIEW).is_ok() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(74)
        };
    }
    #[cfg(target_os = "linux")]
    if let Some(result) = linux::probe_native(&parsed) {
        return match result {
            Ok((text, exit)) => {
                if output::write(&text).is_ok() {
                    ExitCode::from(exit)
                } else {
                    ExitCode::from(74)
                }
            }
            Err(failure) => finish(Err(failure), parsed.json),
        };
    }
    #[cfg(target_os = "linux")]
    let result = linux::run(&parsed);
    #[cfg(not(target_os = "linux"))]
    let result = Err(Failure::new(
        "platform_unavailable",
        "Tailnet local authority integration in fr is currently implemented on Linux.",
        2,
    ));
    finish(result, parsed.json)
}

fn finish(result: Result<String, Failure>, json: bool) -> ExitCode {
    let (text, code) = match result {
        Ok(text) => (text, 0),
        Err(failure) => (terminal::output(failure, json), failure.exit),
    };
    if output::write(&text).is_err() {
        ExitCode::from(74)
    } else {
        ExitCode::from(code)
    }
}
