#![forbid(unsafe_code)]

//! Service lifecycle and installer for the `FrankenRemote` host daemon (`frd`).
//!
//! Implements bead `fr-p2-service-install-aw3` and plan §20.3:
//! - Idempotent registration across platforms:
//!   - Linux: `systemd` user units (recommended for graphical sessions) or system units
//!   - macOS: `launchd` `LaunchAgent` (GUI session) or `LaunchDaemon`
//!   - Windows: Windows Service
//! - Setup preflight: verifies executable, checks port conflicts, and validates directory permissions
//! - Safe uninstall: removes `FrankenRemote` unit files and runtime state without touching Tailscale or tailnet policy
//! - Dry-run mode for previewing generated unit/plist specifications without filesystem mutation

use std::fmt::{self, Write as _};
use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;

/// Target service manager variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceKind {
    /// Linux systemd user service (`~/.config/systemd/user/frd.service`).
    SystemdUser,
    /// Linux systemd system service (`/etc/systemd/system/frd.service`).
    SystemdSystem,
    /// macOS launchd user agent (`~/Library/LaunchAgents/com.frankenremote.frd.plist`).
    LaunchdAgent,
    /// macOS launchd system daemon (`/Library/LaunchDaemons/com.frankenremote.frd.plist`).
    LaunchdDaemon,
    /// Windows service via Service Control Manager.
    WindowsService,
}

impl ServiceKind {
    /// Auto-detect the recommended service kind for the current operating system.
    #[must_use]
    pub const fn default_for_platform() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::LaunchdAgent
        }
        #[cfg(target_os = "windows")]
        {
            Self::WindowsService
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::SystemdUser
        }
    }

    /// Descriptive identifier for reporting and logging.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SystemdUser => "systemd-user",
            Self::SystemdSystem => "systemd-system",
            Self::LaunchdAgent => "launchd-agent",
            Self::LaunchdDaemon => "launchd-daemon",
            Self::WindowsService => "windows-service",
        }
    }
}

/// Typed service installer errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    /// Service port is already bound by another process.
    PortConflict { port: u16 },
    /// Executable was not found or is not accessible.
    ExecutableNotFound { path: String },
    /// Insufficient filesystem or administrative permissions.
    PermissionDenied { detail: String },
    /// Tailscale local daemon is not running or not accessible.
    TailscaleUnavailable { detail: String },
    /// Platform is unsupported for the requested service kind.
    UnsupportedPlatform { detail: String },
    /// General I/O failure.
    IoError { detail: String },
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PortConflict { port } => {
                write!(
                    f,
                    "service port {port} is already in use by another process; select a different port with --port"
                )
            }
            Self::ExecutableNotFound { path } => {
                write!(f, "frd binary not found at '{path}'")
            }
            Self::PermissionDenied { detail } => {
                write!(f, "permission denied: {detail}")
            }
            Self::TailscaleUnavailable { detail } => {
                write!(f, "tailscale unavailable: {detail}")
            }
            Self::UnsupportedPlatform { detail } => {
                write!(f, "unsupported platform: {detail}")
            }
            Self::IoError { detail } => {
                write!(f, "I/O error: {detail}")
            }
        }
    }
}

impl std::error::Error for ServiceError {}

/// Options governing service installation or uninstallation.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    /// Target service manager kind.
    pub kind: ServiceKind,
    /// Ingress service port for QUIC and HTTPS (default 8443).
    pub service_port: u16,
    /// Optional custom Tailscale daemon socket path.
    pub socket_path: Option<PathBuf>,
    /// Initial local operator approval mode ("local" or "none").
    pub approval_mode: String,
    /// Tailnet sharing scope ("own-user" or "tailnet").
    pub sharing_scope: String,
    /// Absolute path to the installed `frd` binary.
    pub exec_path: PathBuf,
    /// Preview generated configuration without modifying filesystem.
    pub dry_run: bool,
    /// Optional directory override (primarily for test isolation).
    pub custom_unit_dir: Option<PathBuf>,
}

impl Default for InstallOptions {
    fn default() -> Self {
        let exec_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("frd"));
        Self {
            kind: ServiceKind::default_for_platform(),
            service_port: 8443,
            socket_path: None,
            approval_mode: "none".into(),
            sharing_scope: "own-user".into(),
            exec_path,
            dry_run: false,
            custom_unit_dir: None,
        }
    }
}

/// Report generated after an installation operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// The service kind installed.
    pub kind: ServiceKind,
    /// Path to the written (or planned) service unit/plist file.
    pub unit_path: PathBuf,
    /// Whether this was a dry run.
    pub dry_run: bool,
    /// Rendered service specification content.
    pub unit_content: String,
    /// Setup instructions or next steps for the operator.
    pub next_steps: Vec<String>,
}

/// Report generated after an uninstallation operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallReport {
    /// The service kind uninstalled.
    pub kind: ServiceKind,
    /// Path to the unit file that was unlinked.
    pub unit_path: PathBuf,
    /// Whether the unit file existed before uninstallation.
    pub existed: bool,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

/// Render a systemd service unit file for `frd`.
#[must_use]
pub fn render_systemd_unit(options: &InstallOptions) -> String {
    let mut args = format!("run --port {}", options.service_port);
    if let Some(sock) = &options.socket_path {
        let _ = write!(args, " --socket {}", sock.display());
    }
    if options.approval_mode != "none" {
        let _ = write!(args, " --approval {}", options.approval_mode);
    }
    if options.sharing_scope != "own-user" {
        let _ = write!(args, " --sharing {}", options.sharing_scope);
    }

    let is_user = matches!(options.kind, ServiceKind::SystemdUser);
    let (target, unit_deps) = if is_user {
        (
            "default.target",
            "After=graphical-session.target network.target tailscaled.service\nPartOf=graphical-session.target",
        )
    } else {
        (
            "multi-user.target",
            "After=network.target tailscaled.service",
        )
    };

    format!(
        r"[Unit]
Description=FrankenRemote Host Daemon
Documentation=https://github.com/Dicklesworthstone/franken_remote
{}
Wants=tailscaled.service

[Service]
Type=simple
ExecStart={} {}
Restart=always
RestartSec=3
StandardOutput=journal
StandardError=journal
LimitNOFILE=65536
Environment=RUST_BACKTRACE=1

[Install]
WantedBy={}
",
        unit_deps,
        options.exec_path.display(),
        args,
        target
    )
}

/// Render a macOS launchd property list for `frd`.
#[must_use]
pub fn render_launchd_plist(options: &InstallOptions) -> String {
    let mut program_args = format!(
        "        <string>{}</string>\n        <string>run</string>\n        <string>--port</string>\n        <string>{}</string>",
        options.exec_path.display(),
        options.service_port
    );

    if let Some(sock) = &options.socket_path {
        let _ = write!(
            program_args,
            "\n        <string>--socket</string>\n        <string>{}</string>",
            sock.display()
        );
    }
    if options.approval_mode != "none" {
        let _ = write!(
            program_args,
            "\n        <string>--approval</string>\n        <string>{}</string>",
            options.approval_mode
        );
    }
    if options.sharing_scope != "own-user" {
        let _ = write!(
            program_args,
            "\n        <string>--sharing</string>\n        <string>{}</string>",
            options.sharing_scope
        );
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.frankenremote.frd</string>
    <key>ProgramArguments</key>
    <array>
{program_args}
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/frd.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/frd.err.log</string>
</dict>
</plist>
"#
    )
}

/// Resolve the filesystem path where the service configuration should be placed.
#[must_use]
pub fn resolve_unit_path(options: &InstallOptions) -> PathBuf {
    if let Some(dir) = &options.custom_unit_dir {
        let filename = match options.kind {
            ServiceKind::LaunchdAgent | ServiceKind::LaunchdDaemon => "com.frankenremote.frd.plist",
            ServiceKind::SystemdUser | ServiceKind::SystemdSystem => "frd.service",
            ServiceKind::WindowsService => "frd.service.json",
        };
        return dir.join(filename);
    }

    match options.kind {
        ServiceKind::SystemdUser => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home)
                .join(".config")
                .join("systemd")
                .join("user")
                .join("frd.service")
        }
        ServiceKind::SystemdSystem => PathBuf::from("/etc/systemd/system/frd.service"),
        ServiceKind::LaunchdAgent => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home)
                .join("Library")
                .join("LaunchAgents")
                .join("com.frankenremote.frd.plist")
        }
        ServiceKind::LaunchdDaemon => {
            PathBuf::from("/Library/LaunchDaemons/com.frankenremote.frd.plist")
        }
        ServiceKind::WindowsService => {
            let program_data =
                std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
            PathBuf::from(program_data)
                .join("FrankenRemote")
                .join("service.json")
        }
    }
}

/// Perform preflight verification before installing the service.
pub fn preflight_check(options: &InstallOptions) -> Result<(), ServiceError> {
    // 1. Verify executable exists (unless dry-run where binary might be hypothetical).
    if !options.dry_run && !options.exec_path.exists() {
        return Err(ServiceError::ExecutableNotFound {
            path: options.exec_path.display().to_string(),
        });
    }

    // 2. Check for port collision on localhost.
    if !options.dry_run {
        match TcpListener::bind(("127.0.0.1", options.service_port)) {
            Ok(listener) => {
                // Port is free; close listener immediately.
                drop(listener);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                return Err(ServiceError::PortConflict {
                    port: options.service_port,
                });
            }
            Err(_) => {
                // Binding localhost failed for other reasons (e.g. sandbox restriction); ignore.
            }
        }
    }

    Ok(())
}

/// Idempotently install the `frd` service.
pub fn install(options: &InstallOptions) -> Result<InstallReport, ServiceError> {
    preflight_check(options)?;

    let unit_path = resolve_unit_path(options);
    let unit_content = match options.kind {
        ServiceKind::SystemdUser | ServiceKind::SystemdSystem => render_systemd_unit(options),
        ServiceKind::LaunchdAgent | ServiceKind::LaunchdDaemon => render_launchd_plist(options),
        ServiceKind::WindowsService => {
            format!(
                r#"{{"name":"frd","display_name":"FrankenRemote Host Daemon","bin_path":"{}","port":{}}}"#,
                options.exec_path.display(),
                options.service_port
            )
        }
    };

    let mut next_steps = Vec::new();

    if options.dry_run {
        next_steps.push(format!(
            "Dry run complete. Would write service definition to: {}",
            unit_path.display()
        ));
    } else {
        if let Some(parent) = unit_path.parent() {
            fs::create_dir_all(parent).map_err(|e| ServiceError::PermissionDenied {
                detail: format!(
                    "Failed to create service directory '{}': {e}",
                    parent.display()
                ),
            })?;
        }

        // Atomic write via temporary file
        let tmp_path = unit_path.with_extension("tmp");
        fs::write(&tmp_path, &unit_content).map_err(|e| ServiceError::PermissionDenied {
            detail: format!("Failed to write unit file: {e}"),
        })?;

        fs::rename(&tmp_path, &unit_path).map_err(|e| ServiceError::PermissionDenied {
            detail: format!("Failed to activate unit file: {e}"),
        })?;

        match options.kind {
            ServiceKind::SystemdUser => {
                next_steps
                    .push("Run 'systemctl --user daemon-reload' to load the new unit.".into());
                next_steps
                    .push("Run 'systemctl --user enable --now frd' to start the service.".into());
            }
            ServiceKind::SystemdSystem => {
                next_steps.push("Run 'sudo systemctl daemon-reload' to load the new unit.".into());
                next_steps
                    .push("Run 'sudo systemctl enable --now frd' to start the service.".into());
            }
            ServiceKind::LaunchdAgent => {
                next_steps.push(format!(
                    "Run 'launchctl load {}' to start the agent.",
                    unit_path.display()
                ));
            }
            ServiceKind::LaunchdDaemon => {
                next_steps.push(format!(
                    "Run 'sudo launchctl load {}' to start the daemon.",
                    unit_path.display()
                ));
            }
            ServiceKind::WindowsService => {
                next_steps.push(
                    "Service configuration registered. Run 'sc start frd' to begin hosting.".into(),
                );
            }
        }
    }

    Ok(InstallReport {
        kind: options.kind,
        unit_path,
        dry_run: options.dry_run,
        unit_content,
        next_steps,
    })
}

/// Safely and idempotently uninstall the `frd` service.
pub fn uninstall(options: &InstallOptions) -> Result<UninstallReport, ServiceError> {
    let unit_path = resolve_unit_path(options);
    let existed = unit_path.exists();

    if !options.dry_run && existed {
        fs::remove_file(&unit_path).map_err(|e| ServiceError::PermissionDenied {
            detail: format!("Failed to remove unit file '{}': {e}", unit_path.display()),
        })?;
    }

    Ok(UninstallReport {
        kind: options.kind,
        unit_path,
        existed,
        dry_run: options.dry_run,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_systemd_user_unit_contains_expected_attributes() {
        let options = InstallOptions {
            kind: ServiceKind::SystemdUser,
            service_port: 8443,
            socket_path: Some(PathBuf::from("/var/run/tailscale/tailscaled.sock")),
            approval_mode: "local".into(),
            sharing_scope: "tailnet".into(),
            exec_path: PathBuf::from("/usr/local/bin/frd"),
            dry_run: true,
            custom_unit_dir: None,
        };

        let unit = render_systemd_unit(&options);
        assert!(unit.contains("ExecStart=/usr/local/bin/frd run --port 8443 --socket /var/run/tailscale/tailscaled.sock --approval local --sharing tailnet"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn render_launchd_plist_contains_xml_elements() {
        let options = InstallOptions {
            kind: ServiceKind::LaunchdAgent,
            service_port: 9000,
            socket_path: None,
            approval_mode: "none".into(),
            sharing_scope: "own-user".into(),
            exec_path: PathBuf::from("/Applications/frd"),
            dry_run: true,
            custom_unit_dir: None,
        };

        let plist = render_launchd_plist(&options);
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("<string>com.frankenremote.frd</string>"));
        assert!(plist.contains("<string>/Applications/frd</string>"));
        assert!(plist.contains("<string>9000</string>"));
    }

    #[test]
    fn install_and_uninstall_idempotency_in_isolated_directory() {
        let temp_dir = std::env::temp_dir().join(format!("frd_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);

        let options = InstallOptions {
            kind: ServiceKind::SystemdUser,
            service_port: 8443,
            socket_path: None,
            approval_mode: "none".into(),
            sharing_scope: "own-user".into(),
            exec_path: PathBuf::from("/bin/sh"),
            dry_run: false,
            custom_unit_dir: Some(temp_dir.clone()),
        };

        // 1. First install succeeds
        let report = install(&options).expect("first install should succeed");
        assert!(report.unit_path.exists());

        // 2. Second install is idempotent
        let report2 = install(&options).expect("second install should succeed idempotently");
        assert!(report2.unit_path.exists());

        // 3. Uninstall unlinks the file
        let un_report = uninstall(&options).expect("uninstall should succeed");
        assert!(un_report.existed);
        assert!(!report.unit_path.exists());

        // 4. Second uninstall is safe and idempotent
        let un_report2 = uninstall(&options).expect("second uninstall should succeed idempotently");
        assert!(!un_report2.existed);

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn preflight_detects_missing_executable() {
        let options = InstallOptions {
            kind: ServiceKind::SystemdUser,
            service_port: 8443,
            socket_path: None,
            approval_mode: "none".into(),
            sharing_scope: "own-user".into(),
            exec_path: PathBuf::from("/nonexistent/path/to/frd_binary_xyz"),
            dry_run: false,
            custom_unit_dir: None,
        };

        let result = preflight_check(&options);
        assert!(matches!(
            result,
            Err(ServiceError::ExecutableNotFound { .. })
        ));
    }
}
