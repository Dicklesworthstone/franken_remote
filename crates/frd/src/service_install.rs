#![forbid(unsafe_code)]

//! Service lifecycle and installer for the `FrankenRemote` host daemon (`frd`).
//!
//! Implements bead `fr-p2-service-install-aw3` and plan §20.3:
//! - Service specifications across platforms:
//!   - Linux: `systemd` user units (recommended for graphical sessions) or system units
//!   - macOS: `launchd` `LaunchAgent` (GUI session) or `LaunchDaemon`
//!   - Windows: Windows Service
//! - Idempotent Linux and macOS publication and removal through verified directory handles
//! - Setup preflight: verifies executable, checks port conflicts, and validates directory permissions
//! - Safe uninstall: removes `FrankenRemote` unit files without touching Tailscale or tailnet policy
//! - Dry-run mode for previewing generated unit/plist specifications without filesystem mutation

use std::fmt::{self, Write as _};

mod options;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod publication;
mod quoting;
#[cfg(unix)]
use std::fs;
use std::net::UdpSocket;
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
    /// A service path is linked, shared-writable, or not owned by its installer.
    UnsafeUnitPath,
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
            Self::UnsafeUnitPath => write!(
                f,
                "unsafe service path: use an installer-owned directory without writable untrusted ancestors, symlinks, or a linked/nonregular existing unit"
            ),
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
    /// Further `frd run` flags (everything after `--`): control, clipboard,
    /// files, audio, display and session-lifetime options. Validated by
    /// `frd run`'s own parser at install time; systemd units only.
    pub run_args: Vec<String>,
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
            run_args: Vec::new(),
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
    /// `None` for a preview. After publication, false means the complete unit
    /// is visible but syncing its directory failed; do not retry as if no
    /// change occurred. A true result includes file and directory durability.
    pub durable: Option<bool>,
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

/// Return the actual protected executable, not the possibly mutable alias used
/// to find it. The root service must execute the same path we verified.
#[cfg(unix)]
fn root_executable(path: &std::path::Path) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    if !path.is_absolute() {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    // The unit renderer accepts UTF-8: a lossy path would name a different
    // executable even when the original alias was a valid CLI argument.
    canonical.to_str()?;
    let executable = fs::symlink_metadata(&canonical).ok()?;
    if !executable.is_file() || executable.mode() & 0o100 == 0 {
        return None;
    }
    canonical
        .ancestors()
        .all(|part| {
            fs::symlink_metadata(part).is_ok_and(|m| {
                m.uid() == 0
                    && m.mode() & 0o022 == 0
                    && !m.file_type().is_symlink()
                    && (part == canonical.as_path() || m.is_dir())
            })
        })
        .then_some(canonical)
}
#[cfg(not(unix))]
fn root_executable(_: &std::path::Path) -> Option<PathBuf> {
    None
}

/// Render the helper's system unit and configuration for local account `uid`.
#[must_use]
pub fn render_ingress_helper(options: &InstallOptions, uid: u32) -> IngressHelperArtifacts {
    let (exec_path, copy_from) = if let Some(canonical) = root_executable(&options.exec_path) {
        (canonical, None)
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
        steps.push(
            "The root helper must use a protected executable, not a user-writable binary.".into(),
        );
        steps.push(format!(
            "[root] sudo install -o root -g root -m 0755 -- {} {}",
            quoting::shell(&source.to_string_lossy()),
            quoting::shell(&helper.exec_path.to_string_lossy())
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
/// The installing account's real uid (the `gui/<uid>` launchd domain).
#[cfg(target_os = "macos")]
#[allow(clippy::unnecessary_wraps)] // The Linux read of the same signature can fail.
fn current_uid() -> Option<u32> {
    Some(rustix::process::getuid().as_raw())
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
    /// `None` when no removal was performed. False preserves a completed
    /// removal whose crash durability could not be confirmed.
    pub durable: Option<bool>,
    /// Removing the unit file does not stop a running service: what to run.
    pub next_steps: Vec<String>,
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
    for arg in &options.run_args {
        let _ = write!(args, " {}", quoting::systemd(arg));
    }

    // A user unit starts with the graphical session (which provides DISPLAY),
    // not default.target (also reached by SSH-only logins, where `frd run` has
    // no display and would only fail). A refusal (exit 2: configuration) is not
    // retried; a runtime failure is.
    let is_user = matches!(options.kind, ServiceKind::SystemdUser);
    let (target, unit_deps) = if is_user {
        (
            "graphical-session.target",
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
Restart=on-failure
RestartPreventExitStatus=2
RestartSec=5
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

/// The launchd job label (and the `LaunchAgents` file stem).
pub const LAUNCHD_LABEL: &str = "com.frankenremote.frd";

/// Where a `LaunchAgent`'s output goes: the user's own log directory, never a
/// shared `/tmp` path another account could pre-create or read.
#[must_use]
pub fn resolve_log_path(options: &InstallOptions) -> PathBuf {
    if let Some(dir) = &options.custom_unit_dir {
        return dir.join("frd.log");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join("Library")
        .join("Logs")
        .join("FrankenRemote")
        .join("frd.log")
}

/// Render a macOS launchd property list for `frd`: a per-user `LaunchAgent`
/// in the login (Aqua) session, whose window server is the shared desktop.
/// launchd restarts a failed start (a permission not granted yet, Tailscale
/// not up yet) after `ThrottleInterval`; a clean exit stays stopped.
#[must_use]
pub fn render_launchd_plist(options: &InstallOptions) -> String {
    fn arg(out: &mut String, value: &str) {
        let _ = write!(out, "\n        <string>{}</string>", quoting::xml(value));
    }
    let mut program_args = String::new();
    arg(&mut program_args, &options.exec_path.to_string_lossy());
    arg(&mut program_args, "run");
    arg(&mut program_args, "--port");
    arg(&mut program_args, &options.service_port.to_string());
    if let Some(sock) = &options.socket_path {
        arg(&mut program_args, "--socket");
        arg(&mut program_args, &sock.to_string_lossy());
    }
    if let Some(path) = &options.config_path {
        arg(&mut program_args, "--config");
        arg(&mut program_args, &path.to_string_lossy());
    }
    if !options.approval_mode.is_empty() {
        arg(&mut program_args, "--approval");
        arg(&mut program_args, &options.approval_mode);
    }
    if !options.sharing_scope.is_empty() {
        arg(&mut program_args, "--sharing");
        arg(&mut program_args, &options.sharing_scope);
    }
    if options.software_explicit {
        arg(&mut program_args, "--software-explicit");
    }
    for value in &options.run_args {
        arg(&mut program_args, value);
    }
    let log = quoting::xml(&resolve_log_path(options).to_string_lossy());

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>{program_args}
    </array>
    <key>LimitLoadToSessionType</key>
    <string>Aqua</string>
    <key>ProcessType</key>
    <string>Interactive</string>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>ThrottleInterval</key>
    <integer>10</integer>
    <key>EnvironmentVariables</key>
    <dict>
        <key>RUST_BACKTRACE</key>
        <string>1</string>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
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

    // 2. Check for a port collision on localhost. frd serves QUIC, so only
    // a UDP holder conflicts; a TCP service on the same number does not.
    if !options.dry_run {
        match UdpSocket::bind(("127.0.0.1", options.service_port)) {
            Ok(socket) => {
                // Port is free; close the probe immediately.
                drop(socket);
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
    let mut durable = None;

    if options.dry_run {
        next_steps.push(format!(
            "Dry run complete. Would write service definition to: {}",
            unit_path.display()
        ));
    } else {
        // launchd opens the log at each start but creates no directory.
        if options.kind == ServiceKind::LaunchdAgent {
            prepare_log_directory(&resolve_log_path(options))?;
        }
        durable = Some(publish_unit(&unit_path, unit_content.as_bytes())?);
        if durable == Some(false) {
            next_steps.push(
                "The complete service definition was published, but directory synchronization failed. Its persistence across a crash is unconfirmed; inspect the installed definition before starting the service."
                    .into(),
            );
        }

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
                next_steps.extend(launchd_agent_steps(
                    &unit_path,
                    &resolve_log_path(options),
                    options,
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

    if options.kind == ServiceKind::SystemdUser && options.approval_mode == "local" {
        next_steps.push(
            "Local approval uses the explicitly selected logind session on every start. Install the matching fr-observation-indicator beside frd and provide the selected session's X11 environment; missing session evidence or UI refuses hosting. After a new login changes the session ID, update the service selection explicitly."
                .into(),
        );
    }

    Ok(InstallReport {
        kind: options.kind,
        unit_path,
        dry_run: options.dry_run,
        durable,
        unit_content,
        next_steps,
        ingress_helper,
    })
}

/// Linux and macOS have a supported persistent host install. Other platforms
/// retain their preview renderers, but cannot write a weaker path than the
/// shipped CLI.
fn publish_unit(path: &std::path::Path, bytes: &[u8]) -> Result<bool, ServiceError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        publication::write(path, bytes)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (path, bytes);
        Err(ServiceError::UnsupportedPlatform {
            detail: "persistent service installation is implemented only on Linux and macOS; use a dry run to preview other service formats".into(),
        })
    }
}

/// The agent's private log directory, created (0700) through the same
/// verified, no-follow directory walk as the definition itself.
fn prepare_log_directory(log: &std::path::Path) -> Result<(), ServiceError> {
    let directory = log.parent().ok_or(ServiceError::UnsafeUnitPath)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        publication::prepare_directory(directory)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = directory;
        Err(ServiceError::UnsupportedPlatform {
            detail: "launchd agents exist only on macOS".into(),
        })
    }
}

/// Install never starts the agent: these are the operator's launchctl steps.
/// `bootstrap` loads the definition into this login session and starts it;
/// launchd never rereads a changed plist, so an update boots the old one out
/// first.
fn launchd_agent_steps(
    unit: &std::path::Path,
    log: &std::path::Path,
    options: &InstallOptions,
) -> Vec<String> {
    let domain =
        current_uid().map_or_else(|| "gui/$(id -u)".to_owned(), |uid| format!("gui/{uid}"));
    let plist = quoting::shell(&unit.to_string_lossy());
    let mut steps = vec![
        format!(
            "Run 'launchctl bootout {domain}/{LAUNCHD_LABEL}' (only if an earlier version is loaded) and then 'launchctl bootstrap {domain} {plist}' to start the agent; it also starts at every login."
        ),
        format!(
            "Restart it with 'launchctl kickstart -k {domain}/{LAUNCHD_LABEL}'; its output goes to {}.",
            log.display()
        ),
        "On its first start macOS asks for Screen Recording for frd (and Accessibility when it controls input). Allow frd under System Settings > Privacy & Security; frd retries every 10 seconds until both are granted, and a signed frd keeps the grants across updates.".into(),
    ];
    if options.run_args.iter().all(|arg| arg != "--input-agent") {
        steps.push(
            "This agent shares the desktop view-only: install with -- --input-agent PATH to allow control.".into(),
        );
    }
    steps
}

/// Safely and idempotently uninstall the `frd` service.
pub fn uninstall(options: &InstallOptions) -> Result<UninstallReport, ServiceError> {
    let unit_path = resolve_unit_path(options);
    let (existed, durable) = remove_unit(&unit_path, options.dry_run)?;

    let mut next_steps = match options.kind {
        ServiceKind::SystemdUser => vec![
            "[user] Removing the unit file does not stop a running service: run 'systemctl --user disable --now frd' and then 'systemctl --user daemon-reload'.".into(),
        ],
        ServiceKind::SystemdSystem => vec![
            "Removing the unit file does not stop a running service: run 'sudo systemctl disable --now frd' and then 'sudo systemctl daemon-reload'.".into(),
        ],
        ServiceKind::LaunchdAgent => vec![format!(
            "Removing the definition does not stop a loaded agent: run 'launchctl bootout {}/{LAUNCHD_LABEL}'.",
            current_uid().map_or_else(|| "gui/$(id -u)".to_owned(), |uid| format!("gui/{uid}"))
        )],
        _ => vec![
            "Removing the service definition does not stop a running frd: unload or stop it with the platform service manager.".into(),
        ],
    };
    if durable == Some(false) {
        next_steps.push(
            "The service definition was removed, but directory synchronization failed. Its removal across a crash is unconfirmed; inspect the definition before restarting the service manager."
                .into(),
        );
    }
    Ok(UninstallReport {
        kind: options.kind,
        unit_path,
        existed,
        dry_run: options.dry_run,
        durable,
        next_steps,
    })
}

fn remove_unit(
    path: &std::path::Path,
    preview: bool,
) -> Result<(bool, Option<bool>), ServiceError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        publication::remove(path, preview)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        if preview {
            return Ok((path.exists(), None));
        }
        Err(ServiceError::UnsupportedPlatform {
            detail: "persistent service removal is implemented only on Linux and macOS; use a dry run to preview other service formats".into(),
        })
    }
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
            run_args: Vec::new(),
        };

        let unit = render_systemd_unit(&options);
        assert!(unit.contains("ExecStart=:/usr/local/bin/frd run --port 8443 --socket /var/run/tailscale/tailscaled.sock --approval local --sharing tailnet"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("RestartPreventExitStatus=2"));
        assert!(unit.contains("WantedBy=graphical-session.target"));
        assert!(!unit.contains("default.target"));
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
            run_args: Vec::new(),
        };

        let plist = render_launchd_plist(&options);
        assert!(plist.contains("<key>Label</key>"));
        assert!(plist.contains("<string>com.frankenremote.frd</string>"));
        assert!(plist.contains("<string>/Applications/frd</string>"));
        assert!(plist.contains("<string>9000</string>"));
        assert!(plist.contains("<string>--software-explicit</string>"));
        // The login session's window server, restarted only after a failure.
        assert!(plist.contains("<key>LimitLoadToSessionType</key>\n    <string>Aqua</string>"));
        assert!(plist.contains("<key>ProcessType</key>\n    <string>Interactive</string>"));
        assert!(plist.contains(
            "<key>KeepAlive</key>\n    <dict>\n        <key>SuccessfulExit</key>\n        <false/>"
        ));
        assert!(plist.contains("<key>ThrottleInterval</key>\n    <integer>10</integer>"));
        // Output goes to the user's own log directory, never a shared /tmp.
        assert!(!plist.contains("/tmp/"));
        assert!(plist.contains("Library/Logs/FrankenRemote/frd.log</string>"));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn launchd_agent_install_replaces_and_uninstall_removes_only_its_plist() {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        struct Root(PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        let mut random = [0; 16];
        getrandom::fill(&mut random).unwrap();
        // The no-follow walk refuses macOS's /var symlink: use the real path.
        let root = Root(
            std::env::temp_dir()
                .canonicalize()
                .unwrap()
                .join(format!("frd-launchd-{:032x}", u128::from_ne_bytes(random))),
        );
        fs::DirBuilder::new().mode(0o700).create(&root.0).unwrap();
        let foreign = root.0.join("com.example.other.plist");
        fs::write(&foreign, b"another agent").unwrap();
        let mut options = InstallOptions {
            kind: ServiceKind::LaunchdAgent,
            exec_path: PathBuf::from("/bin/sh"),
            approval_mode: "none".into(),
            software_explicit: true,
            custom_unit_dir: Some(root.0.clone()),
            ..InstallOptions::default()
        };
        // Bind to no fixed port: the preflight port probe is not under test.
        options.service_port = {
            let probe = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
            probe.local_addr().unwrap().port()
        };
        let first = install(&options).unwrap();
        assert_eq!(first.unit_path, root.0.join("com.frankenremote.frd.plist"));
        assert_eq!(first.durable, Some(true));
        let written = fs::read_to_string(&first.unit_path).unwrap();
        assert_eq!(written, first.unit_content);
        let metadata = fs::symlink_metadata(&first.unit_path).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.mode() & 0o077, 0, "a private, unshared plist");
        assert!(
            first
                .next_steps
                .iter()
                .any(|s| s.contains("launchctl bootstrap gui/"))
        );
        assert!(first.next_steps.iter().any(|s| s.contains("view-only")));

        // A changed definition replaces the whole file atomically.
        options.approval_mode = String::new();
        let second = install(&options).unwrap();
        assert_eq!(
            fs::read_to_string(&second.unit_path).unwrap(),
            second.unit_content
        );
        assert!(!second.unit_content.contains("--approval"));

        let removed = uninstall(&options).unwrap();
        assert!(removed.existed);
        assert!(!removed.unit_path.exists());
        assert!(removed.next_steps[0].contains("launchctl bootout gui/"));
        assert!(!uninstall(&options).unwrap().existed);
        assert_eq!(fs::read(&foreign).unwrap(), b"another agent");
        let leftovers: Vec<_> = fs::read_dir(&root.0)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(
            leftovers,
            [std::ffi::OsString::from("com.example.other.plist")]
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
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
            run_args: Vec::new(),
        };

        // 1. First install succeeds
        let report = install(&options).expect("first install should succeed");
        assert!(report.unit_path.exists());
        assert_eq!(report.durable, Some(true));

        // 2. Second install is idempotent
        let report2 = install(&options).expect("second install should succeed idempotently");
        assert!(report2.unit_path.exists());
        assert_eq!(report2.durable, Some(true));

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
            run_args: Vec::new(),
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
            let canonical = PathBuf::from("/usr/bin/env").canonicalize().unwrap();
            assert_eq!(direct.exec_path, canonical);
            assert!(direct.unit_content.contains(&format!(
                "ExecStart=:{} ingress-helper",
                quoting::systemd(canonical.to_str().unwrap())
            )));
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
    fn preflight_refuses_a_udp_holder_but_not_a_tcp_service_on_the_port() {
        let holder = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let port = holder.local_addr().unwrap().port();
        let mut options = InstallOptions {
            exec_path: PathBuf::from("/bin/sh"),
            service_port: port,
            software_explicit: true,
            ..InstallOptions::default()
        };
        assert_eq!(
            preflight_check(&options),
            Err(ServiceError::PortConflict { port })
        );
        drop(holder);
        // QUIC never conflicts with a TCP listener of the same number.
        let tcp = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        options.service_port = tcp.local_addr().unwrap().port();
        if UdpSocket::bind(("127.0.0.1", options.service_port)).is_ok() {
            assert_eq!(preflight_check(&options), Ok(()));
        }
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
            run_args: Vec::new(),
        };

        let result = preflight_check(&options);
        assert!(matches!(
            result,
            Err(ServiceError::ExecutableNotFound { .. })
        ));
    }

    #[test]
    #[cfg(unix)]
    fn helper_copy_instructions_preserve_paths_as_literal_shell_arguments() {
        let source =
            PathBuf::from("/home/example/frd with 'quote'; $(printf expanded); `printf expanded`");
        let options = InstallOptions {
            exec_path: source.clone(),
            ..InstallOptions::default()
        };
        let helper = render_ingress_helper(&options, 1000);
        assert_eq!(helper.copy_from, Some(source.clone()));
        let steps = ingress_helper_steps(&helper);
        let args = steps
            .iter()
            .find_map(|step| step.strip_prefix("[root] sudo install "))
            .unwrap();
        // Parse only the quoted arguments using a harmless fixed printf. The
        // generated sudo/install command is never executed by this regression.
        let output = std::process::Command::new("/bin/sh")
            .args(["-c", &format!("set -- {args}; printf '%s\\000' \"$@\"")])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stderr, [] as [u8; 0]);
        let actual: Vec<_> = output.stdout.split(|byte| *byte == 0).collect();
        assert_eq!(
            actual,
            [
                b"-o".as_slice(),
                b"root",
                b"-g",
                b"root",
                b"-m",
                b"0755",
                b"--",
                source.as_os_str().as_encoded_bytes(),
                helper.exec_path.as_os_str().as_encoded_bytes(),
                b"",
            ]
        );
    }
}
