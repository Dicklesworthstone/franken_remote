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

mod options;
mod quoting;
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
    /// Invalid or ambiguous local service configuration.
    InvalidOptions,
    /// The installed `frd run` would refuse this profile at every start (and
    /// the service manager would restart it forever), so installation refuses.
    HostProfileUnavailable {
        code: &'static str,
        detail: &'static str,
    },
    /// General I/O failure.
    IoError { detail: String },
}

impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOptions => {
                write!(f, "invalid service options; no service change was applied")
            }
            Self::HostProfileUnavailable { code, detail } => write!(f, "{code}: {detail}"),
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
    /// Optional saved host policy file loaded by the service at startup.
    pub config_path: Option<PathBuf>,
    /// Explicit approval override ("local" or "none"); empty inherits saved policy.
    pub approval_mode: String,
    /// Explicit sharing override ("own-user" or "tailnet"); empty inherits saved policy.
    pub sharing_scope: String,
    /// Absolute path to the installed `frd` binary.
    pub exec_path: PathBuf,
    /// Preview generated configuration without modifying filesystem.
    pub dry_run: bool,
    /// Optional directory override (primarily for test isolation).
    pub custom_unit_dir: Option<PathBuf>,
    /// Pass `--software-explicit` to `frd run` (the CPU HEVC developer profile;
    /// `frd run` has no hardware encoder selection yet and refuses without it).
    pub software_explicit: bool,
}

impl Default for InstallOptions {
    fn default() -> Self {
        let exec_path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("frd"));
        Self {
            kind: ServiceKind::default_for_platform(),
            service_port: 8443,
            socket_path: None,
            config_path: None,
            approval_mode: String::new(),
            sharing_scope: String::new(),
            exec_path,
            dry_run: false,
            custom_unit_dir: None,
            software_explicit: false,
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
    /// Root-side ingress helper files a user-unit `frd run` needs. They are
    /// rendered for the administrator, never written by an unprivileged install.
    pub ingress_helper: Option<IngressHelperArtifacts>,
}

/// Must equal `fr_tailnet::ingress::helper::DEFAULT_CONFIG` (checked by a test).
pub const INGRESS_HELPER_CONFIG: &str = "/etc/frankenremote/ingress-helper.json";
pub const INGRESS_HELPER_UNIT: &str = "/etc/systemd/system/frd-ingress-helper.service";
/// Where the helper's root-only copy of `frd` goes when the installing binary
/// lives in a user-writable tree (a root service must never run such a file).
pub const INGRESS_HELPER_EXEC: &str = "/usr/local/bin/frd";

/// The root half of an unprivileged Linux install: a system unit running
/// `frd ingress-helper`, and the root-owned configuration admitting the uid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressHelperArtifacts {
    pub unit_path: PathBuf,
    pub unit_content: String,
    pub config_path: PathBuf,
    pub config_content: String,
    /// The binary the unit runs; `copy_from` is set when it must first be
    /// installed there from the (not root-only) installing executable.
    pub exec_path: PathBuf,
    pub copy_from: Option<PathBuf>,
}

/// Whether `path` and every ancestor are root-owned and not group/other-writable.
#[cfg(unix)]
fn root_only(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    path.is_absolute()
        && path.canonicalize().is_ok_and(|path| {
            path.ancestors().all(|part| {
                fs::symlink_metadata(part).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0)
            })
        })
}
#[cfg(not(unix))]
fn root_only(_: &std::path::Path) -> bool {
    false
}

/// Render the helper's system unit and configuration for local account `uid`.
#[must_use]
pub fn render_ingress_helper(options: &InstallOptions, uid: u32) -> IngressHelperArtifacts {
    let (exec_path, copy_from) = if root_only(&options.exec_path) {
        (options.exec_path.clone(), None)
    } else {
        (
            PathBuf::from(INGRESS_HELPER_EXEC),
            Some(options.exec_path.clone()),
        )
    };
    let unit_content = format!(
        r"[Unit]
Description=FrankenRemote ingress helper (root owner of frd's nftables ingress rule)
Documentation=https://github.com/Dicklesworthstone/franken_remote
After=network.target tailscaled.service

[Service]
Type=simple
ExecStart=:{} ingress-helper --config {INGRESS_HELPER_CONFIG}
Restart=on-failure
RestartSec=2
RuntimeDirectory=frankenremote-ingress
RuntimeDirectoryMode=0755
UMask=0022
NoNewPrivileges=yes
CapabilityBoundingSet=CAP_NET_ADMIN
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
",
        quoting::systemd(&exec_path.to_string_lossy()),
    );
    IngressHelperArtifacts {
        unit_path: PathBuf::from(INGRESS_HELPER_UNIT),
        unit_content,
        config_path: PathBuf::from(INGRESS_HELPER_CONFIG),
        config_content: format!("{{\"interface\":\"tailscale0\",\"allowed_uids\":[{uid}]}}\n"),
        exec_path,
        copy_from,
    }
}

/// Operator steps for the helper, each labelled with the account it needs.
fn ingress_helper_steps(helper: &IngressHelperArtifacts) -> Vec<String> {
    let mut steps = vec![
        "Ingress enforcement needs root once: this user service has no CAP_NET_ADMIN, so \
         'frd run' asks the root ingress helper for its nftables rule and refuses \
         (ingress_helper_unavailable) until the helper runs."
            .to_owned(),
    ];
    if let Some(source) = &helper.copy_from {
        steps.push(format!(
            "[root] sudo install -o root -g root -m 0755 {} {} (a root service must not run a user-writable binary)",
            source.display(),
            helper.exec_path.display()
        ));
    }
    steps.push(format!(
        "[root] Write {} (owner root, mode 0644) with the helper configuration shown below.",
        helper.config_path.display()
    ));
    steps.push(format!(
        "[root] Write {} with the helper unit shown below.",
        helper.unit_path.display()
    ));
    steps.push(
        "[root] Run 'sudo systemctl daemon-reload && sudo systemctl enable --now frd-ingress-helper'."
            .to_owned(),
    );
    steps
}

/// The installing account's real uid, from the kernel's status file.
#[cfg(target_os = "linux")]
fn current_uid() -> Option<u32> {
    fs::read_to_string("/proc/self/status").ok().and_then(|s| {
        s.lines()
            .find_map(|line| line.strip_prefix("Uid:"))
            .and_then(|ids| ids.split_whitespace().next()?.parse().ok())
    })
}
#[cfg(not(target_os = "linux"))]
fn current_uid() -> Option<u32> {
    None
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
        let _ = write!(
            args,
            " --socket {}",
            quoting::systemd(&sock.to_string_lossy())
        );
    }
    if let Some(path) = &options.config_path {
        let _ = write!(
            args,
            " --config {}",
            quoting::systemd(&path.to_string_lossy())
        );
    }
    if !options.approval_mode.is_empty() {
        let _ = write!(
            args,
            " --approval {}",
            quoting::systemd(&options.approval_mode)
        );
    }
    if !options.sharing_scope.is_empty() {
        let _ = write!(
            args,
            " --sharing {}",
            quoting::systemd(&options.sharing_scope)
        );
    }
    if options.software_explicit {
        args.push_str(" --software-explicit");
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
ExecStart=:{} {}
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
        quoting::systemd(&options.exec_path.to_string_lossy()),
        args,
        target
    )
}

/// Render a macOS launchd property list for `frd`.
#[must_use]
pub fn render_launchd_plist(options: &InstallOptions) -> String {
    let mut program_args = format!(
        "        <string>{}</string>\n        <string>run</string>\n        <string>--port</string>\n        <string>{}</string>",
        quoting::xml(&options.exec_path.to_string_lossy()),
        options.service_port
    );

    if let Some(sock) = &options.socket_path {
        let _ = write!(
            program_args,
            "\n        <string>--socket</string>\n        <string>{}</string>",
            quoting::xml(&sock.to_string_lossy())
        );
    }
    if let Some(path) = &options.config_path {
        let _ = write!(
            program_args,
            "\n        <string>--config</string>\n        <string>{}</string>",
            quoting::xml(&path.to_string_lossy())
        );
    }
    if !options.approval_mode.is_empty() {
        let _ = write!(
            program_args,
            "\n        <string>--approval</string>\n        <string>{}</string>",
            quoting::xml(&options.approval_mode)
        );
    }
    if !options.sharing_scope.is_empty() {
        let _ = write!(
            program_args,
            "\n        <string>--sharing</string>\n        <string>{}</string>",
            quoting::xml(&options.sharing_scope)
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
    options.validate()?;
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
                next_steps.push(
                    "[user] Run 'systemctl --user daemon-reload' to load the new unit.".into(),
                );
                next_steps.push(
                    "[user] Run 'systemctl --user enable --now frd' to start the service.".into(),
                );
            }
            ServiceKind::SystemdSystem => {
                next_steps.push("Run 'sudo systemctl daemon-reload' to load the new unit.".into());
                next_steps
                    .push("Run 'sudo systemctl enable --now frd' to start the service.".into());
                next_steps.push(
                    "This system unit runs frd as root, which installs its ingress rule \
                     directly; no ingress helper is needed."
                        .into(),
                );
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

    // A user unit cannot administer nftables; the root half is rendered for
    // the administrator (a root user's own unit installs its rule directly).
    let ingress_helper = (options.kind == ServiceKind::SystemdUser)
        .then(current_uid)
        .flatten()
        .filter(|uid| *uid != 0)
        .map(|uid| render_ingress_helper(options, uid));
    if let Some(helper) = &ingress_helper {
        next_steps.extend(ingress_helper_steps(helper));
    }

    Ok(InstallReport {
        kind: options.kind,
        unit_path,
        dry_run: options.dry_run,
        unit_content,
        next_steps,
        ingress_helper,
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
            config_path: None,
            approval_mode: "local".into(),
            sharing_scope: "tailnet".into(),
            exec_path: PathBuf::from("/usr/local/bin/frd"),
            dry_run: true,
            custom_unit_dir: None,
            software_explicit: true,
        };

        let unit = render_systemd_unit(&options);
        assert!(unit.contains("ExecStart=:/usr/local/bin/frd run --port 8443 --socket /var/run/tailscale/tailscaled.sock --approval local --sharing tailnet"));
        assert!(unit.contains("Restart=always"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    #[test]
    fn render_launchd_plist_contains_xml_elements() {
        let options = InstallOptions {
            kind: ServiceKind::LaunchdAgent,
            service_port: 9000,
            socket_path: None,
            config_path: None,
            approval_mode: "none".into(),
            sharing_scope: "own-user".into(),
            exec_path: PathBuf::from("/Applications/frd"),
            dry_run: true,
            custom_unit_dir: None,
            software_explicit: true,
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
            config_path: None,
            approval_mode: "none".into(),
            sharing_scope: "own-user".into(),
            exec_path: PathBuf::from("/bin/sh"),
            dry_run: false,
            custom_unit_dir: Some(temp_dir.clone()),
            software_explicit: true,
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
    fn a_user_unit_install_renders_the_root_ingress_helper_without_writing_it() {
        let options = InstallOptions {
            kind: ServiceKind::SystemdUser,
            exec_path: PathBuf::from("/home/someone/.cargo/bin/frd"),
            dry_run: true,
            software_explicit: true,
            approval_mode: "none".into(),
            ..InstallOptions::default()
        };
        let helper = render_ingress_helper(&options, 1000);
        assert_eq!(
            helper.config_content,
            "{\"interface\":\"tailscale0\",\"allowed_uids\":[1000]}\n"
        );
        // A user-writable installer binary is never what the root unit runs.
        assert_eq!(helper.copy_from, Some(options.exec_path.clone()));
        for line in [
            "ExecStart=:/usr/local/bin/frd ingress-helper --config /etc/frankenremote/ingress-helper.json",
            "RuntimeDirectory=frankenremote-ingress",
            "RuntimeDirectoryMode=0755",
            "CapabilityBoundingSet=CAP_NET_ADMIN",
            "NoNewPrivileges=yes",
            "RestartSec=2",
            "WantedBy=multi-user.target",
        ] {
            assert!(helper.unit_content.lines().any(|l| l == line), "{line}");
        }
        #[cfg(unix)]
        {
            let root = InstallOptions {
                exec_path: PathBuf::from("/usr/bin/env"),
                ..options.clone()
            };
            let direct = render_ingress_helper(&root, 1000);
            assert_eq!(direct.copy_from, None);
            assert!(
                direct
                    .unit_content
                    .contains("ExecStart=:/usr/bin/env ingress-helper")
            );
        }
        #[cfg(target_os = "linux")]
        {
            use fr_tailnet::ingress::helper;
            let settings = helper::Settings::parse(helper.config_content.as_bytes()).unwrap();
            assert!(settings.allows(1000) && !settings.allows(1001));
            assert_eq!(helper::DEFAULT_CONFIG, INGRESS_HELPER_CONFIG);
            assert_eq!(
                settings.socket().parent(),
                Some(std::path::Path::new("/run/frankenremote-ingress"))
            );
        }
        let report = install(&options).unwrap();
        if current_uid().is_some_and(|uid| uid != 0) {
            let rendered = report.ingress_helper.as_ref().unwrap();
            assert_eq!(rendered.unit_path, PathBuf::from(INGRESS_HELPER_UNIT));
            let root_steps = report
                .next_steps
                .iter()
                .filter(|s| s.starts_with("[root]"))
                .count();
            assert_eq!(root_steps, 4, "{:?}", report.next_steps);
            assert!(
                report
                    .next_steps
                    .iter()
                    .any(|s| s.contains("ingress_helper_unavailable"))
            );
        }
        let system = install(&InstallOptions {
            kind: ServiceKind::SystemdSystem,
            ..options
        })
        .unwrap();
        assert_eq!(system.ingress_helper, None);
    }

    #[test]
    fn preflight_detects_missing_executable() {
        let options = InstallOptions {
            kind: ServiceKind::SystemdUser,
            service_port: 8443,
            socket_path: None,
            config_path: None,
            approval_mode: "none".into(),
            sharing_scope: "own-user".into(),
            exec_path: PathBuf::from("/nonexistent/path/to/frd_binary_xyz"),
            dry_run: false,
            custom_unit_dir: None,
            software_explicit: true,
        };

        let result = preflight_check(&options);
        assert!(matches!(
            result,
            Err(ServiceError::ExecutableNotFound { .. })
        ));
    }
}
