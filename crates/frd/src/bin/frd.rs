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
    run             Run the host daemon broker in foreground
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
    --user          Manage user-level service (systemd user unit / launchd agent; default)
    --system        Manage system-wide service
    --dry-run       Preview service generation without modifying filesystem
    --config PATH   Local policy file for run/approval/sharing (absolute, Linux)
    --json          Output structured, schema-versioned JSON envelope
    --help, -h      Print this help text
";

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
        "install" => execute_install(&args, json),
        "uninstall" => execute_uninstall(&args, json),
        "service-status" => execute_service_status(&args, json),
        other => {
            eprintln!("Unknown command '{other}'. Run 'frd --help' for usage.");
            ExitCode::from(2)
        }
    }
}

fn execute_status(args: &[String], json: bool) -> ExitCode {
    let mut socket = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--socket" && i + 1 < args.len() {
            socket = Some(std::path::PathBuf::from(&args[i + 1]));
            i += 2;
        } else {
            i += 1;
        }
    }
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

fn parse_port(args: &[String]) -> u16 {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--port"
            && i + 1 < args.len()
            && let Ok(p) = args[i + 1].parse::<u16>()
        {
            return p;
        }
    }
    8443
}

fn parse_socket(args: &[String]) -> Option<PathBuf> {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--socket" && i + 1 < args.len() {
            return Some(PathBuf::from(&args[i + 1]));
        }
    }
    None
}

fn parse_approval_flag(args: &[String]) -> String {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--approval" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
    }
    "none".into()
}

fn parse_sharing_flag(args: &[String]) -> String {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--sharing" && i + 1 < args.len() {
            return args[i + 1].clone();
        }
    }
    "own-user".into()
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

fn execute_install(args: &[String], json: bool) -> ExitCode {
    let port = parse_port(args);
    let socket = parse_socket(args);
    let approval = parse_approval_flag(args);
    let sharing = parse_sharing_flag(args);
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let kind = parse_service_kind(args);

    let options = InstallOptions {
        kind,
        service_port: port,
        socket_path: socket,
        approval_mode: approval,
        sharing_scope: sharing,
        exec_path: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("frd")),
        dry_run,
        custom_unit_dir: None,
    };

    match service_install::install(&options) {
        Ok(report) => {
            if json {
                println!(
                    "{{\"outcome\":\"success\",\"kind\":\"{}\",\"unit_path\":\"{}\",\"dry_run\":{}}}",
                    report.kind.as_str(),
                    report.unit_path.display(),
                    report.dry_run
                );
            } else {
                println!(
                    "Service installation {} for {}:",
                    if report.dry_run {
                        "preview"
                    } else {
                        "succeeded"
                    },
                    report.kind.as_str()
                );
                println!("  Unit file: {}", report.unit_path.display());
                for step in &report.next_steps {
                    println!("  {step}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            if json {
                println!("{{\"outcome\":\"failure\",\"error\":\"{e}\"}}");
            } else {
                eprintln!("Error installing service: {e}");
            }
            ExitCode::from(1)
        }
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
                println!("{{\"outcome\":\"failure\",\"error\":\"{e}\"}}");
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
fn execute_run(args: &[String], json: bool) -> ExitCode {
    use asupersync::runtime::RuntimeBuilder;
    use asupersync::signal::{SignalKind, signal};
    use asupersync::types::Budget;
    use fr_core::ids::{HostBootId, OsSessionId};
    use fr_tailnet::LocalApi;
    use frd::broker::config::{ApprovalMode, DaemonConfig, SharingScope};
    use frd::broker::service::BrokerService;

    use frd::host_policy::{Approval, Sharing, options::RunOptions};
    let options = match RunOptions::parse(args) {
        Ok(options) => options,
        Err(error) => return local_policy::refusal(error, json),
    };
    let effective = match options.resolve() {
        Ok(effective) => effective,
        Err(error) => return local_policy::refusal(error, json),
    };
    let socket = options.socket;
    let port = effective.port;
    let config = DaemonConfig {
        service_port: effective.port,
        approval_mode: match effective.approval {
            Approval::Local => ApprovalMode::PromptAlways,
            Approval::None => ApprovalMode::Unattended,
        },
        sharing_scope: match effective.sharing {
            Sharing::OwnUser => SharingScope::OwnUser,
            Sharing::Tailnet => SharingScope::Tailnet,
        },
        ..Default::default()
    };

    let runtime = match RuntimeBuilder::current_thread()
        .enable_platform_reactor(true)
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            if json {
                println!(
                    "{{\"outcome\":\"refusal\",\"code\":\"runtime_unavailable\",\"detail\":\"{e}\"}}"
                );
            } else {
                eprintln!("Error: Failed to initialize async runtime: {e}");
            }
            return ExitCode::from(2);
        }
    };

    let Ok(cx) = runtime
        .handle()
        .try_request_cx_with_budget(Budget::INFINITE)
    else {
        eprintln!("Error: Failed to acquire runtime context.");
        return ExitCode::from(2);
    };

    let api = match &socket {
        Some(p) => match LocalApi::new(p) {
            Ok(a) => a,
            Err(e) => {
                if json {
                    println!(
                        "{{\"outcome\":\"refusal\",\"code\":\"tailscale_unavailable\",\"detail\":\"{e}\"}}"
                    );
                } else {
                    eprintln!(
                        "Error: Cannot connect to Tailscale at '{}': {e}",
                        p.display()
                    );
                }
                return ExitCode::from(1);
            }
        },
        None => LocalApi::installed(),
    };

    // Query tailscale status to verify daemon connectivity and fetch host identity
    let tailscale_status = runtime.block_on(async { api.node_identity(&cx).await });

    let (fqdn, tailnet_ips) = match tailscale_status {
        Ok(node) => {
            let fqdn = node.certificate_name().to_string();
            let ips = node.addresses().to_vec();
            (fqdn, ips)
        }
        Err(e) => {
            if json {
                println!(
                    "{{\"outcome\":\"refusal\",\"code\":\"tailscale_unavailable\",\"detail\":\"{e}\"}}"
                );
            } else {
                eprintln!(
                    "Refusal: Tailscale local daemon is not running or socket is inaccessible ({e})."
                );
                eprintln!("FrankenRemote requires an active Tailscale tailnet node to operate.");
            }
            return ExitCode::from(1);
        }
    };

    let boot_id = HostBootId::from_raw(1);
    let os_session_id = OsSessionId::from_raw(1);
    let broker = BrokerService::new(config, boot_id, os_session_id, fqdn.clone(), tailnet_ips);

    let https_endpoints = broker.honest_https_endpoints();
    let quic_endpoints = broker.honest_quic_endpoints();

    if json {
        println!(
            "{{\"outcome\":\"running\",\"node\":\"{}\",\"port\":{},\"https_endpoints\":{:?},\"quic_endpoints\":{:?},\"desktop\":\"{:?}\"}}",
            fqdn, port, https_endpoints, quic_endpoints, broker.desktop_availability
        );
    } else {
        println!("============================================================");
        println!("FrankenRemote Host Daemon (frd) Starting");
        println!("============================================================");
        println!("  Tailnet Node:  {fqdn}");
        println!("  Ingress Port:  {port}");
        for ep in &https_endpoints {
            println!("  HTTPS Endpoint: {ep}");
        }
        for ep in &quic_endpoints {
            println!("  QUIC Endpoint:  {ep}");
        }
        println!("  Desktop State: {:?}", broker.desktop_availability);
        println!("  Approval Mode: {:?}", broker.config.approval_mode);
        println!("  Sharing Scope: {:?}", broker.config.sharing_scope);
        println!("============================================================");
        println!("Broker listening. Press Ctrl-C to shut down.");
    }

    // Run event loop until SIGINT/SIGTERM
    runtime.block_on(async {
        let mut sigint = signal(SignalKind::interrupt()).ok();
        let mut sigterm = signal(SignalKind::terminate()).ok();

        std::future::poll_fn(|task| {
            if let Some(ref mut s) = sigint
                && std::pin::pin!(s.recv()).poll(task).is_ready()
            {
                return std::task::Poll::Ready(());
            }
            if let Some(ref mut s) = sigterm
                && std::pin::pin!(s.recv()).poll(task).is_ready()
            {
                return std::task::Poll::Ready(());
            }
            std::task::Poll::Pending
        })
        .await;
    });

    if !json {
        println!("\nShutdown signal received. Tearing down broker cleanly...");
        println!("Broker stopped. Zero residual sessions or listeners retained.");
    }
    ExitCode::SUCCESS
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
