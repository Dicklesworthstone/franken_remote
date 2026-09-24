#![forbid(unsafe_code)]

//! Host daemon command-line entry point.
//!
//! Provides the administrative, service lifecycle, and daemon runtime commands:
//! - `frd status [--json]`: typed failure explanations, capability matrix, sessions
//! - `frd approval [get | set local | set none]`: local observer/controller approval mode
//! - `frd sharing [get | set own-user | set tailnet]`: tailnet sharing admission scope
//! - `frd run [--port 8443] [--socket /path/to/tailscaled.sock] [--approval local|none] [--sharing own-user|tailnet] [--json]`
//! - `frd install [--user | --system] [--dry-run] [--port 8443]`
//! - `frd uninstall [--user | --system] [--dry-run]`
//! - `frd service-status [--user | --system] [--json]`
//!
//! Matches plan §§5.1, 18.1, 20.3 and beads `fr-p2-doctor-diagnostics-bo4` and `fr-p2-service-install-aw3`.

use frd::service_install::{self, InstallOptions, ServiceKind};
use frd::status::DaemonStatusReport;
use std::path::PathBuf;
use std::process::ExitCode;

const HELP: &str = "\
FrankenRemote host daemon management tool

USAGE:
    frd run [OPTIONS]
    frd status [--json]
    frd approval [get | set local | set none]
    frd sharing [get | set own-user | set tailnet]
    frd install [--user | --system] [--dry-run] [--port PORT]
    frd uninstall [--user | --system] [--dry-run]
    frd service-status [--user | --system] [--json]
    frd --help

COMMANDS:
    run             Share this desktop view-only over the tailnet (foreground; needs root for ingress)
    status          Report host daemon health, capabilities, sessions, and restrictions
    approval        Inspect or update local operator approval policy
    sharing         Inspect or update tailnet sharing admission scope
    install         Install frd as an idempotent background service (systemd / launchd)
    uninstall       Remove installed service and unit files cleanly
    service-status  Check whether background service is registered and active

OPTIONS:
    --port PORT     Ingress port for QUIC and HTTPS (default: 8443)
    --socket PATH   Path to tailscaled.sock
    --approval MODE Initial approval mode: 'local' (prompt) or 'none' (unattended)
    --sharing SCOPE Sharing scope: 'own-user' (default) or 'tailnet'
    --headless      Share a private headless Xvfb display (cookie-authenticated)
    --display :N    X11 display to share (default: $DISPLAY)
    --worker PATH   Absolute fr-media-worker path (default: next to frd)
    --interface IF  Tailscale interface for ingress enforcement (default: tailscale0)
    --trust-roots P PEM CA bundle for the host certificate chain (default: system)
    --once          Serve one sharing session, then exit
    --software-explicit  Encode HEVC on the CPU (developer profile; required by
                    frd run until hardware encoder selection exists)
    --user          Manage user-level service (systemd user unit / launchd agent; default)
    --system        Manage system-wide service
    --dry-run       Preview service generation without modifying filesystem
    --config PATH   Local policy file for run/install/approval/sharing (absolute, Linux)
    --json          Output structured, schema-versioned JSON envelope
    --help, -h      Print this help text
";

#[path = "frd/install.rs"]
mod local_install;

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
    let Some(command_index) = args.iter().position(|arg| arg != "--json") else {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    };
    let cmd = args[command_index].as_str();
    let args: Vec<String> = args
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != command_index)
        .map(|(_, arg)| arg.clone())
        .collect();

    match cmd {
        "status" => execute_status(&args, json),
        "approval" => execute_policy(&args, true, json),
        "sharing" => execute_policy(&args, false, json),
        "run" => execute_run(&args, json),
        "install" => local_install::execute(&args, json),
        "uninstall" => execute_uninstall(&args, json),
        "service-status" => execute_service_status(&args, json),
        other => {
            eprintln!("Unknown command '{other}'. Run 'frd --help' for usage.");
            ExitCode::from(2)
        }
    }
}

fn execute_status(args: &[String], json: bool) -> ExitCode {
    // Always a live probe; a legacy `--live` flag is accepted and ignored.
    let socket = args
        .windows(2)
        .find(|pair| pair[0] == "--socket")
        .map(|pair| std::path::PathBuf::from(&pair[1]));
    let report = DaemonStatusReport::probe_host(socket.as_deref());
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

#[cfg(target_os = "linux")]
#[path = "frd/policy.rs"]
mod local_policy;

fn execute_policy(args: &[String], approval: bool, json: bool) -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        local_policy::execute(args, approval, json)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (args, approval, json);
        eprintln!("Persistent host policy is not implemented on this platform.");
        ExitCode::from(2)
    }
}

fn parse_service_kind(args: &[String]) -> ServiceKind {
    if args.iter().any(|a| a == "--system") {
        #[cfg(target_os = "macos")]
        {
            ServiceKind::LaunchdDaemon
        }
        #[cfg(target_os = "windows")]
        {
            ServiceKind::WindowsService
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            ServiceKind::SystemdSystem
        }
    } else {
        ServiceKind::default_for_platform()
    }
}

fn execute_uninstall(args: &[String], json: bool) -> ExitCode {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let kind = parse_service_kind(args);

    let options = InstallOptions {
        kind,
        dry_run,
        exec_path: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("frd")),
        ..Default::default()
    };

    match service_install::uninstall(&options) {
        Ok(report) => {
            if json {
                println!(
                    "{{\"outcome\":\"success\",\"kind\":\"{}\",\"unit_path\":\"{}\",\"existed\":{},\"dry_run\":{}}}",
                    report.kind.as_str(),
                    report.unit_path.display(),
                    report.existed,
                    report.dry_run
                );
            } else {
                println!(
                    "Service uninstallation {} for {}:",
                    if report.dry_run {
                        "preview"
                    } else {
                        "completed"
                    },
                    report.kind.as_str()
                );
                println!("  Target path: {}", report.unit_path.display());
                if report.existed {
                    println!("  Service configuration removed.");
                } else {
                    println!("  Service was not previously installed (idempotent no-op).");
                }
                println!("  Note: Tailscale and user credentials left untouched.");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({"outcome": "failure", "error": e.to_string()})
                );
            } else {
                eprintln!("Error uninstalling service: {e}");
            }
            ExitCode::from(1)
        }
    }
}

fn execute_service_status(args: &[String], json: bool) -> ExitCode {
    let kind = parse_service_kind(args);
    let options = InstallOptions {
        kind,
        ..Default::default()
    };
    let unit_path = service_install::resolve_unit_path(&options);
    let installed = unit_path.exists();

    if json {
        println!(
            "{{\"kind\":\"{}\",\"unit_path\":\"{}\",\"installed\":{}}}",
            kind.as_str(),
            unit_path.display(),
            installed
        );
    } else {
        println!("Service Registration Status:");
        println!("  Manager:   {}", kind.as_str());
        println!("  Unit file: {}", unit_path.display());
        println!("  Installed: {}", if installed { "yes" } else { "no" });
    }

    if installed {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

#[allow(clippy::too_many_lines)]
#[cfg(target_os = "linux")]
fn run_refusal(json: bool, code: &str, detail: &str, status: u8) -> ExitCode {
    if json {
        println!(
            "{}",
            serde_json::json!({"outcome": "refusal", "code": code, "detail": detail})
        );
    } else {
        eprintln!("Refusal ({code}): {detail}");
    }
    ExitCode::from(status)
}

#[cfg(target_os = "linux")]
fn print_event(json: bool, event: &frd::host_run::Event) {
    use frd::host_run::Event;

    if json {
        let value = match event {
            Event::Listening { address } => {
                serde_json::json!({"event": "listening", "address": address.to_string()})
            }
            Event::PeerFinished {
                attempts,
                admitted,
                refused,
                outcome,
            } => serde_json::json!({"event": "peer_finished", "attempts": attempts,
                "admitted": admitted, "refused": refused, "outcome": outcome}),
            Event::ShareEnded { outcome } => {
                serde_json::json!({"event": "share_ended", "outcome": outcome})
            }
            Event::CleanupFailed { stage } => {
                serde_json::json!({"event": "cleanup_failed", "stage": stage})
            }
            Event::Stopped => serde_json::json!({"event": "stopped"}),
        };
        println!("{value}");
    } else {
        match event {
            Event::Listening { address } => {
                println!("frd: sharing this desktop (view-only) on {address}; Ctrl-C to stop");
            }
            Event::PeerFinished { outcome, .. } => println!("frd: peer finished: {outcome}"),
            Event::ShareEnded { outcome } => println!("frd: share ended: {outcome}"),
            Event::CleanupFailed { stage } => eprintln!("frd: cleanup failed: {stage}"),
            Event::Stopped => println!("frd: stopped"),
        }
    }
}

#[cfg(target_os = "linux")]
type SelectedDesktop = (
    String,
    Option<PathBuf>,
    Option<frd::broker::virtual_display::VirtualDisplayInstance>,
);

/// The X11 display to share: a private cookie-authenticated Xvfb for
/// `--headless`, otherwise `--display` or the inherited DISPLAY/XAUTHORITY.
#[cfg(target_os = "linux")]
fn select_desktop(
    options: &frd::host_policy::options::RunOptions,
    json: bool,
) -> Result<SelectedDesktop, ExitCode> {
    use frd::broker::virtual_display::{VirtualDisplayConfig, VirtualDisplayManager};
    if options.headless {
        return match VirtualDisplayManager::start(&VirtualDisplayConfig {
            enabled: true,
            ..VirtualDisplayConfig::default()
        }) {
            Ok(instance) => Ok((
                instance.display.clone(),
                instance.xauthority.clone(),
                Some(instance),
            )),
            Err(error) => Err(run_refusal(
                json,
                "virtual_display_failed",
                &error.to_string(),
                1,
            )),
        };
    }
    match options
        .display
        .clone()
        .or_else(|| std::env::var("DISPLAY").ok())
    {
        Some(display) => Ok((
            display,
            std::env::var_os("XAUTHORITY").map(PathBuf::from),
            None,
        )),
        None => Err(run_refusal(
            json,
            "no_display",
            "no X11 display to share: set DISPLAY, pass --display, or use --headless",
            2,
        )),
    }
}

#[cfg(target_os = "linux")]
fn execute_run(args: &[String], json: bool) -> ExitCode {
    use frd::host_policy::{Approval, Sharing, options::RunOptions};
    use frd::host_run::{self, Event, Options, Reporter};
    use std::sync::Arc;

    let options = match RunOptions::parse(args) {
        Ok(options) => options,
        Err(error) => return local_policy::refusal(error, json),
    };
    let effective = match options.resolve() {
        Ok(effective) => effective,
        Err(error) => return local_policy::refusal(error, json),
    };
    // Tailscale first: without it nothing else matters (exit 1 = runtime refusal).
    let tailscale = options
        .socket
        .clone()
        .unwrap_or_else(|| PathBuf::from("/var/run/tailscale/tailscaled.sock"));
    if !tailscale.exists() {
        return run_refusal(
            json,
            "tailscale_unavailable",
            "the installed Tailscale LocalAPI socket is missing; start tailscaled",
            1,
        );
    }
    if effective.approval == Approval::Local {
        return run_refusal(
            json,
            "local_approval_unavailable",
            "local approval needs the interactive session-agent process, which frd run does not \
             host yet; set `frd approval set none` to share unattended (scope stays as configured)",
            2,
        );
    }
    if !options.software_explicit {
        return run_refusal(
            json,
            "hardware_hevc_unavailable",
            "frd run has no hardware HEVC encoder selection yet; pass --software-explicit to \
             share with the CPU software profile (docs/decisions/0004-software-encoder-profile.md)",
            2,
        );
    }
    // Keep the Xvfb owner alive for the whole run; dropping it stops the server
    // and removes its private cookie.
    let (display, xauthority, headless) = match select_desktop(&options, json) {
        Ok(selected) => selected,
        Err(code) => return code,
    };
    let worker = options.worker.clone().or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("fr-media-worker")))
    });
    let Some(worker) = worker.filter(|path| path.is_absolute() && path.is_file()) else {
        return run_refusal(
            json,
            "worker_unavailable",
            "fr-media-worker not found; build fr-native with --features linux-media and pass \
             --worker /absolute/path/fr-media-worker",
            2,
        );
    };
    let run_options = Options {
        socket: options.socket.clone(),
        port: effective.port,
        interface: options
            .interface
            .clone()
            .unwrap_or_else(|| "tailscale0".into()),
        worker,
        display,
        xauthority,
        trust_roots: options
            .trust_roots
            .clone()
            .unwrap_or_else(|| PathBuf::from(fr_tailnet::trust::SYSTEM_BUNDLE)),
        sharing: match effective.sharing {
            Sharing::OwnUser => fr_tailnet::Scope::OwnUser,
            Sharing::Tailnet => fr_tailnet::Scope::Tailnet,
        },
        fps: 30,
        bitrate: 8_000_000,
        ingress_tools: None,
        once: options.once,
        handle_signals: true,
    };
    let report: Reporter = Arc::new(move |event: Event| print_event(json, &event));
    let stop = Arc::new(host_run::StopHandle::default());
    let result = host_run::run(&run_options, &report, &stop);
    drop(headless);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let detail = error.to_string();
            run_refusal(json, error.code(), &detail, 1)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn execute_run(_args: &[String], json: bool) -> ExitCode {
    if json {
        println!(
            "{{\"outcome\":\"refusal\",\"code\":\"platform_unavailable\",\"detail\":\"frd host runtime is currently supported on Linux.\"}}"
        );
    } else {
        eprintln!("Refusal: frd host runtime is currently supported on Linux.");
    }
    ExitCode::from(2)
}
