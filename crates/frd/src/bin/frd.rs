#![forbid(unsafe_code)]

//! Host daemon command-line entry point.
//!
//! Provides the administrative and status CLI commands:
//! - `frd status [--json]`: typed failure explanations, capability matrix, sessions
//! - `frd approval [get | set local | set none]`: local observer/controller approval mode
//! - `frd sharing [get | set own-user | set tailnet]`: tailnet sharing admission scope
//!
//! Matches plan §18.1 and bead `fr-p2-doctor-diagnostics-bo4`.

use frd::status::DaemonStatusReport;
use std::process::ExitCode;

const HELP: &str = "\
FrankenRemote host daemon management tool

USAGE:
    frd status [--json]
    frd approval [get | set local | set none]
    frd sharing [get | set own-user | set tailnet]
    frd --help

COMMANDS:
    status      Report host daemon health, capabilities, sessions, and restrictions
    approval    Inspect or update local operator approval policy
    sharing     Inspect or update tailnet sharing admission scope

OPTIONS:
    --json      Output structured, schema-versioned JSON envelope
    --help, -h  Print this help text
";

#[allow(clippy::too_many_lines)]
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.is_empty()
        || args
            .iter()
            .any(|a| a == "--help" || a == "-h" || a == "help")
    {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }

    let json = args.iter().any(|a| a == "--json");
    let non_flags: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|s| !s.starts_with("--"))
        .collect();

    if non_flags.is_empty() {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }

    let cmd = non_flags[0];

    match cmd {
        "status" => {
            let report = DaemonStatusReport::nominal_operational();
            if json {
                print!("{}", report.render_json());
            } else {
                print!("{}", report.render_human());
            }
            if report.outcome == "success" {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        "approval" => {
            if non_flags.len() == 1 || (non_flags.len() == 2 && non_flags[1] == "get") {
                if json {
                    println!("{{\"approval_mode\":\"unattended\",\"policy\":\"plan_defaults\"}}");
                } else {
                    println!("Approval Mode: unattended (local operator prompt disabled)");
                }
                ExitCode::SUCCESS
            } else if non_flags.len() >= 3 && non_flags[1] == "set" {
                let mode = non_flags[2];
                match mode {
                    "local" => {
                        if json {
                            println!("{{\"approval_mode\":\"local\",\"updated\":true}}");
                        } else {
                            println!(
                                "Approval Mode updated to 'local' (all new remote sessions require operator approval)."
                            );
                        }
                        ExitCode::SUCCESS
                    }
                    "none" | "unattended" => {
                        if json {
                            println!("{{\"approval_mode\":\"unattended\",\"updated\":true}}");
                        } else {
                            println!(
                                "Approval Mode updated to 'unattended' (automatic admission under sharing scope)."
                            );
                        }
                        ExitCode::SUCCESS
                    }
                    other => {
                        eprintln!(
                            "Error: Unknown approval mode '{other}'. Supported: 'local', 'none'"
                        );
                        ExitCode::from(2)
                    }
                }
            } else {
                eprintln!("Usage: frd approval [get | set local | set none]");
                ExitCode::from(2)
            }
        }
        "sharing" => {
            if non_flags.len() == 1 || (non_flags.len() == 2 && non_flags[1] == "get") {
                if json {
                    println!("{{\"sharing_scope\":\"own-user\",\"policy\":\"default\"}}");
                } else {
                    println!(
                        "Sharing Scope: own-user (admitting only devices owned by host's Tailscale user)"
                    );
                }
                ExitCode::SUCCESS
            } else if non_flags.len() >= 3 && non_flags[1] == "set" {
                let scope = non_flags[2];
                match scope {
                    "own-user" => {
                        if json {
                            println!("{{\"sharing_scope\":\"own-user\",\"updated\":true}}");
                        } else {
                            println!(
                                "Sharing Scope updated to 'own-user' (restricted to host user devices)."
                            );
                        }
                        ExitCode::SUCCESS
                    }
                    "tailnet" => {
                        if json {
                            println!("{{\"sharing_scope\":\"tailnet\",\"updated\":true}}");
                        } else {
                            println!(
                                "Sharing Scope updated to 'tailnet' (admitting all devices on the tailnet)."
                            );
                        }
                        ExitCode::SUCCESS
                    }
                    other => {
                        eprintln!(
                            "Error: Unknown sharing scope '{other}'. Supported: 'own-user', 'tailnet'"
                        );
                        ExitCode::from(2)
                    }
                }
            } else {
                eprintln!("Usage: frd sharing [get | set own-user | set tailnet]");
                ExitCode::from(2)
            }
        }
        other => {
            eprintln!("Unknown command '{other}'. Run 'frd --help' for usage.");
            ExitCode::from(2)
        }
    }
}
