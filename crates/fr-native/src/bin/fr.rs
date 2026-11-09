#![forbid(unsafe_code)]
// Other platforms retain shared grammar/output; only implemented paths dispatch.
// The macOS desktop client (fr-5a4w) runs hosts, status, connect and the
// authenticated local close owner; robot automation and screenshots stay Linux-only.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]
//! Native CLI entry point. Hosting is never enabled by installing/running it.
#[path = "fr_cli/closure.rs"]
mod closure;
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fr_cli/discovery.rs"]
mod discovery;
#[path = "fr_cli/file_commands.rs"]
mod file_commands;
#[cfg(any(
    all(target_os = "linux", feature = "linux-desktop"),
    all(target_os = "macos", feature = "macos-desktop")
))]
#[path = "fr_cli/file_reports.rs"]
mod file_reports;
#[path = "fr_cli/help.rs"]
mod help;
#[cfg(any(
    all(target_os = "linux", feature = "linux-desktop"),
    all(target_os = "macos", feature = "macos-desktop")
))]
#[path = "fr_cli/host_choice.rs"]
mod host_choice;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-desktop")
))]
#[path = "fr_cli/linux.rs"]
mod linux;
#[cfg(any(all(target_os = "macos", not(feature = "macos-desktop")), test))]
#[path = "fr_cli/macos_inspect.rs"]
mod macos_inspect;
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[path = "fr_cli/runtime.rs"]
mod runtime;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-desktop")
))]
#[path = "fr_cli/saved_hosts.rs"]
mod saved_hosts;
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-desktop")
))]
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
#[allow(clippy::too_many_lines)]
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
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "macos-desktop")
    ))]
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
    #[cfg(any(
        target_os = "linux",
        all(target_os = "macos", feature = "macos-desktop")
    ))]
    let result = linux::run(&parsed);
    #[cfg(all(target_os = "macos", not(feature = "macos-desktop")))]
    let result = match &parsed.command {
        options::Command::Hosts => discovery::run(&parsed),
        options::Command::Inspect(_) => macos_inspect::run(&parsed),
        _ => Err(Failure::new(
            "platform_unavailable",
            "This macOS build supports fr hosts and fr inspect for installed Tailscale identity; build fr-native with the macos-desktop feature for fr connect and fr status.",
            2,
        )),
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
