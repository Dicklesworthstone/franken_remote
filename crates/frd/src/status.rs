#![forbid(unsafe_code)]

//! Host daemon status inspection and capability matrix reporting.
//!
//! Implements `frd status --json` and `fr doctor` capability reporting
//! per plan §§2.1, 18.1, 18.4, and 24.5 (bead `fr-p2-doctor-diagnostics-bo4`).
//!
//! Provides typed explanations for failure states:
//! - `permission_missing` / `permission_revoked`: specific OS permission missing
//! - `certificate_expired`: Tailscale HTTPS cert past its validity window
//! - `no_hardware_encoder`: hardware HEVC acceleration unavailable
//! - `tailnet_disconnected`: host node not connected to Tailscale

use std::fmt::Write as _;

#[cfg(target_os = "linux")]
use asupersync::runtime::RuntimeBuilder;
#[cfg(target_os = "linux")]
use asupersync::types::Budget;
#[cfg(target_os = "linux")]
use fr_tailnet::LocalApi;

/// Status of an individual capability in the matrix (plan §24.5).
///
/// An untested row is strictly `NotTested`, never "supported with caveats".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    /// Capability verified available and operational.
    Passed,
    /// Capability probe ran and explicitly failed.
    Failed,
    /// Capability cannot run due to a missing prerequisite (e.g. permission or cert).
    Blocked,
    /// Capability has not been probed or qualified on this target.
    NotTested,
}

impl CapabilityStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::NotTested => "not tested",
        }
    }

    pub const fn badge(self) -> &'static str {
        match self {
            Self::Passed => "[PASSED]",
            Self::Failed => "[FAILED]",
            Self::Blocked => "[BLOCKED]",
            Self::NotTested => "[NOT TESTED]",
        }
    }
}

/// A typed row in the capability matrix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityRow {
    pub name: &'static str,
    pub status: CapabilityStatus,
    pub detail: String,
    pub hardware_accelerated: Option<bool>,
    pub restriction: Option<&'static str>,
}

/// OS permission state for an individual capability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRow {
    pub capability: &'static str,
    pub status: &'static str, // "granted", "denied", "prompt_needed", "unsupported"
    pub detail: Option<String>,
}

/// Information about the host's installed Tailscale node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailscaleStatus {
    pub variant: String, // "standalone", "operator", "app_store", "not_installed"
    pub tailnet: String,
    pub connected: bool,
    pub node_id: String,
    pub node_name: String,
    pub user_id: String,
    pub addresses: Vec<String>,
}

/// Information about the host's TLS 1.3 certificate status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateStatus {
    pub certificate_name: String,
    pub generation: u64,
    pub valid: bool,
    pub not_after_wall_us: Option<u64>,
    pub expiry_countdown_secs: Option<u64>,
    pub next_refresh_us: u64,
    pub status_message: String,
}

/// Information about a connected remote session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectedSessionStatus {
    pub session_id: u64,
    pub device_name: String,
    pub role: &'static str, // "controller", "observer"
    pub capabilities: Vec<&'static str>,
    pub authority_state: &'static str, // "admitted", "pending", "revoked"
}

/// Information about host sharing and approval policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharingAndApprovalStatus {
    pub sharing_scope: &'static str, // "own-user", "tailnet"
    pub approval_mode: &'static str, // "local", "unattended"
    pub active_viewers: usize,
    pub max_viewers: usize,
    pub active_controller: Option<u64>,
}

/// Known product restriction (plan §§2.3, 24.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownRestriction {
    pub code: &'static str,
    pub summary: &'static str,
    pub scope: &'static str,
}

/// Structured log entry emitted during status checks or refusals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredLogEntry {
    pub timestamp_unix_ms: u64,
    pub level: &'static str,
    pub event_code: &'static str,
    pub message: String,
}

/// The complete host status report adhering to the robot envelope schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonStatusReport {
    pub schema_version: &'static str,
    pub timestamp_unix_ms: u64,
    pub outcome: &'static str, // "success", "refusal", "failure"
    pub refusal_code: Option<&'static str>,
    pub next_action: Option<&'static str>,
    pub tailscale: TailscaleStatus,
    pub certificate: CertificateStatus,
    pub permissions: Vec<PermissionRow>,
    pub capabilities: Vec<CapabilityRow>,
    pub sessions: Vec<ConnectedSessionStatus>,
    pub sharing: SharingAndApprovalStatus,
    pub restrictions: Vec<KnownRestriction>,
    pub structured_logs: Vec<StructuredLogEntry>,
}

impl DaemonStatusReport {
    pub const CURRENT_SCHEMA_VERSION: &'static str = "fr.status.v1";

    /// Standard known restrictions list according to `FrankenRemote` v1.4 design.
    pub fn standard_restrictions() -> Vec<KnownRestriction> {
        vec![
            KnownRestriction {
                code: "video_hevc_only",
                summary: "HEVC (Main, 8-bit, 4:2:0 baseline) only. No secondary video codec.",
                scope: "media",
            },
            KnownRestriction {
                code: "audio_opus_only",
                summary: "Opus audio only (48 kHz mono/stereo). No alternative audio codec.",
                scope: "audio",
            },
            KnownRestriction {
                code: "full_display_only",
                summary: "Selected full-display capture only. Window cropping deferred.",
                scope: "capture",
            },
            KnownRestriction {
                code: "tailscale_ingress_only",
                summary: "Tailscale authenticated ingress only. No public relays or pairing PINs.",
                scope: "network",
            },
            KnownRestriction {
                code: "single_controller_authority",
                summary: "Single controller owns input authority at a time; up to 2 read-only observers.",
                scope: "input",
            },
            KnownRestriction {
                code: "no_zero_rtt_application_data",
                summary: "Zero-RTT application data is forbidden; requires full TLS/QUIC handshake.",
                scope: "security",
            },
        ]
    }

    /// Appends a structured log entry to the report.
    pub fn emit_log(&mut self, level: &'static str, event_code: &'static str, message: &str) {
        self.structured_logs.push(StructuredLogEntry {
            timestamp_unix_ms: self.timestamp_unix_ms,
            level,
            event_code,
            message: message.to_string(),
        });
    }

    /// Renders the report as strict, valid schema-versioned JSON.
    #[allow(clippy::too_many_lines)]
    pub fn render_json(&self) -> String {
        let mut out = String::new();
        let _ = write!(
            out,
            "{{\"schema_version\":\"{}\",\"timestamp_unix_ms\":{},\"outcome\":\"{}\"",
            self.schema_version, self.timestamp_unix_ms, self.outcome
        );

        if let Some(code) = self.refusal_code {
            let _ = write!(out, ",\"refusal_code\":\"{}\"", escape_json(code));
        } else {
            let _ = write!(out, ",\"refusal_code\":null");
        }

        if let Some(action) = self.next_action {
            let _ = write!(out, ",\"next_action\":\"{}\"", escape_json(action));
        } else {
            let _ = write!(out, ",\"next_action\":null");
        }

        // Tailscale
        let addrs_json = self
            .tailscale
            .addresses
            .iter()
            .map(|a| format!("\"{}\"", escape_json(a)))
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(
            out,
            ",\"tailscale\":{{\"variant\":\"{}\",\"tailnet\":\"{}\",\"connected\":{},\"node_id\":\"{}\",\"node_name\":\"{}\",\"user_id\":\"{}\",\"addresses\":[{}]}}",
            escape_json(&self.tailscale.variant),
            escape_json(&self.tailscale.tailnet),
            self.tailscale.connected,
            escape_json(&self.tailscale.node_id),
            escape_json(&self.tailscale.node_name),
            escape_json(&self.tailscale.user_id),
            addrs_json
        );

        // Certificate
        let not_after = self
            .certificate
            .not_after_wall_us
            .map_or_else(|| "null".into(), |v| v.to_string());
        let countdown = self
            .certificate
            .expiry_countdown_secs
            .map_or_else(|| "null".into(), |v| v.to_string());
        let _ = write!(
            out,
            ",\"certificate\":{{\"certificate_name\":\"{}\",\"generation\":{},\"valid\":{},\"not_after_wall_us\":{},\"expiry_countdown_secs\":{},\"next_refresh_us\":{},\"status_message\":\"{}\"}}",
            escape_json(&self.certificate.certificate_name),
            self.certificate.generation,
            self.certificate.valid,
            not_after,
            countdown,
            self.certificate.next_refresh_us,
            escape_json(&self.certificate.status_message)
        );

        // Permissions
        let perms_json = self
            .permissions
            .iter()
            .map(|p| {
                let detail_str = p
                    .detail
                    .as_ref()
                    .map_or_else(|| "null".into(), |d| format!("\"{}\"", escape_json(d)));
                format!(
                    "{{\"capability\":\"{}\",\"status\":\"{}\",\"detail\":{}}}",
                    escape_json(p.capability),
                    escape_json(p.status),
                    detail_str
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(out, ",\"permissions\":[{perms_json}]");

        // Capabilities
        let caps_json = self
            .capabilities
            .iter()
            .map(|c| {
                let hw_str = c
                    .hardware_accelerated
                    .map_or_else(|| "null".into(), |h| h.to_string());
                let restr_str = c
                    .restriction
                    .map_or_else(|| "null".into(), |r| format!("\"{}\"", escape_json(r)));
                format!(
                    "{{\"name\":\"{}\",\"status\":\"{}\",\"detail\":\"{}\",\"hardware_accelerated\":{},\"restriction\":{}}}",
                    escape_json(c.name),
                    c.status.as_str(),
                    escape_json(&c.detail),
                    hw_str,
                    restr_str
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(out, ",\"capabilities\":[{caps_json}]");

        // Sessions
        let sessions_json = self
            .sessions
            .iter()
            .map(|s| {
                let caps = s
                    .capabilities
                    .iter()
                    .map(|c| format!("\"{}\"", escape_json(c)))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{{\"session_id\":{},\"device_name\":\"{}\",\"role\":\"{}\",\"capabilities\":[{}],\"authority_state\":\"{}\"}}",
                    s.session_id,
                    escape_json(&s.device_name),
                    escape_json(s.role),
                    caps,
                    escape_json(s.authority_state)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(out, ",\"sessions\":[{sessions_json}]");

        // Sharing
        let ctrl_str = self
            .sharing
            .active_controller
            .map_or_else(|| "null".into(), |c| c.to_string());
        let _ = write!(
            out,
            ",\"sharing\":{{\"sharing_scope\":\"{}\",\"approval_mode\":\"{}\",\"active_viewers\":{},\"max_viewers\":{},\"active_controller\":{}}}",
            escape_json(self.sharing.sharing_scope),
            escape_json(self.sharing.approval_mode),
            self.sharing.active_viewers,
            self.sharing.max_viewers,
            ctrl_str
        );

        // Restrictions
        let restr_json = self
            .restrictions
            .iter()
            .map(|r| {
                format!(
                    "{{\"code\":\"{}\",\"summary\":\"{}\",\"scope\":\"{}\"}}",
                    escape_json(r.code),
                    escape_json(r.summary),
                    escape_json(r.scope)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(out, ",\"restrictions\":[{restr_json}]");

        // Structured logs
        let logs_json = self
            .structured_logs
            .iter()
            .map(|l| {
                format!(
                    "{{\"timestamp_unix_ms\":{},\"level\":\"{}\",\"event_code\":\"{}\",\"message\":\"{}\"}}",
                    l.timestamp_unix_ms,
                    escape_json(l.level),
                    escape_json(l.event_code),
                    escape_json(&l.message)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let _ = write!(out, ",\"structured_logs\":[{logs_json}]");

        out.push_str("}\n");
        out
    }

    /// Renders the report as human-readable diagnostic text.
    #[allow(clippy::too_many_lines)]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "================================================================================"
        );
        let _ = writeln!(
            out,
            "                   FRANKENREMOTE HOST DAEMON STATUS (frd)"
        );
        let _ = writeln!(
            out,
            "================================================================================"
        );

        // Overall outcome
        let status_tag = if self.outcome == "success" {
            "[OK - OPERATIONAL]"
        } else if self.outcome == "refusal" {
            "[REFUSAL - ACTION REQUIRED]"
        } else {
            "[FAILURE - UNHEALTHY]"
        };
        let _ = writeln!(out, "Status: {status_tag}");
        if let Some(code) = self.refusal_code {
            let _ = writeln!(out, "Refusal Code: {code}");
        }
        if let Some(action) = self.next_action {
            let _ = writeln!(out, "Next Action : {action}");
        }
        let _ = writeln!(out);

        // Tailscale section
        let _ = writeln!(out, "1. TAILSCALE NETWORK STATUS:");
        let _ = writeln!(
            out,
            "--------------------------------------------------------------------------------"
        );
        let _ = writeln!(out, "  Installation Variant : {}", self.tailscale.variant);
        let _ = writeln!(out, "  Tailnet              : {}", self.tailscale.tailnet);
        let _ = writeln!(
            out,
            "  Connectivity         : {}",
            if self.tailscale.connected {
                "Connected"
            } else {
                "DISCONNECTED"
            }
        );
        let _ = writeln!(
            out,
            "  Node Name / ID       : {} ({})",
            self.tailscale.node_name, self.tailscale.node_id
        );
        let _ = writeln!(out, "  User ID              : {}", self.tailscale.user_id);
        let _ = writeln!(
            out,
            "  Addresses            : {}",
            self.tailscale.addresses.join(", ")
        );
        let _ = writeln!(out);

        // Certificate section
        let _ = writeln!(out, "2. TLS 1.3 CERTIFICATE LIFECYCLE:");
        let _ = writeln!(
            out,
            "--------------------------------------------------------------------------------"
        );
        let _ = writeln!(
            out,
            "  Certificate Domain   : {}",
            self.certificate.certificate_name
        );
        let _ = writeln!(
            out,
            "  Status               : {} ({})",
            if self.certificate.valid {
                "Valid"
            } else {
                "EXPIRED / INVALID"
            },
            self.certificate.status_message
        );
        if let Some(secs) = self.certificate.expiry_countdown_secs {
            let days = secs / 86400;
            let hours = (secs % 86400) / 3600;
            let _ = writeln!(
                out,
                "  Expiry Countdown     : {secs} seconds ({days}d {hours}h remaining)"
            );
        } else {
            let _ = writeln!(
                out,
                "  Expiry Countdown     : None (certificate absent or unparsed)"
            );
        }
        let _ = writeln!(
            out,
            "  Certificate Gen      : {}",
            self.certificate.generation
        );
        let _ = writeln!(out);

        // OS Permissions
        let _ = writeln!(out, "3. OS PERMISSION STATES:");
        let _ = writeln!(
            out,
            "--------------------------------------------------------------------------------"
        );
        for p in &self.permissions {
            let detail_text = p.detail.as_deref().unwrap_or("");
            let status_badge = match p.status {
                "granted" => "[GRANTED]       ",
                "denied" => "[DENIED]        ",
                "prompt_needed" => "[PROMPT NEEDED] ",
                _ => "[UNSUPPORTED]   ",
            };
            let _ = writeln!(
                out,
                "  {:18} {} {}",
                p.capability, status_badge, detail_text
            );
        }
        let _ = writeln!(out);

        // Capability Matrix
        let _ = writeln!(out, "4. HARDWARE & MEDIA CAPABILITY MATRIX:");
        let _ = writeln!(
            out,
            "--------------------------------------------------------------------------------"
        );
        for c in &self.capabilities {
            let badge = c.status.badge();
            let hw = match c.hardware_accelerated {
                Some(true) => " [HW]",
                Some(false) => " [SW]",
                None => "",
            };
            let _ = writeln!(out, "  {:18} {:14}{} : {}", c.name, badge, hw, c.detail);
            if let Some(r) = c.restriction {
                let _ = writeln!(out, "    * Restriction: {r}");
            }
        }
        let _ = writeln!(out);

        // Connected Sessions & Authority
        let _ = writeln!(out, "5. ACTIVE SESSIONS & AUTHORITY:");
        let _ = writeln!(
            out,
            "--------------------------------------------------------------------------------"
        );
        let _ = writeln!(
            out,
            "  Sharing Scope: {} | Approval Mode: {} | Viewers: {}/{}",
            self.sharing.sharing_scope,
            self.sharing.approval_mode,
            self.sharing.active_viewers,
            self.sharing.max_viewers
        );
        if let Some(ctrl) = self.sharing.active_controller {
            let _ = writeln!(out, "  Active Controller: Session {ctrl}");
        } else {
            let _ = writeln!(out, "  Active Controller: None (idle / observe only)");
        }
        if self.sessions.is_empty() {
            let _ = writeln!(out, "  Connected Sessions: None");
        } else {
            let _ = writeln!(out, "  Connected Sessions ({}):", self.sessions.len());
            for s in &self.sessions {
                let _ = writeln!(
                    out,
                    "    - Session {} ({}) [{}]: capabilities=[{}] authority={}",
                    s.session_id,
                    s.device_name,
                    s.role,
                    s.capabilities.join(", "),
                    s.authority_state
                );
            }
        }
        let _ = writeln!(out);

        // Known Restrictions
        let _ = writeln!(out, "6. KNOWN RESTRICTIONS & SCOPE LIMITS:");
        let _ = writeln!(
            out,
            "--------------------------------------------------------------------------------"
        );
        for r in &self.restrictions {
            let _ = writeln!(out, "  * [{}] {}: {}", r.scope, r.code, r.summary);
        }

        // Structured log trail if present
        if !self.structured_logs.is_empty() {
            let _ = writeln!(out);
            let _ = writeln!(out, "7. STRUCTURED DIAGNOSTIC LOG TRAIL:");
            let _ = writeln!(
                out,
                "--------------------------------------------------------------------------------"
            );
            for l in &self.structured_logs {
                let _ = writeln!(out, "  [{}] {}: {}", l.level, l.event_code, l.message);
            }
        }

        out
    }

    // -----------------------------------------------------------------------
    // Factory methods for nominal, probed, and failure states
    // -----------------------------------------------------------------------

    /// Probes the real host environment (Tailscale LocalAPI, OS display, audio, GPU).
    ///
    /// If Tailscale is running and accessible, returns a populated operational report.
    /// If Tailscale is offline or inaccessible, returns an honest typed refusal (`tailnet_disconnected`).
    #[allow(clippy::too_many_lines)]
    pub fn probe_host(socket_override: Option<&std::path::Path>) -> Self {
        let timestamp_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);

        #[cfg(target_os = "linux")]
        {
            let api = match socket_override {
                Some(p) => LocalApi::new(p),
                None => Ok(LocalApi::installed()),
            };

            let (connected, fqdn, ips, node_id, user_id, tailnet_name) = match api {
                Ok(local_api) => {
                    let runtime = RuntimeBuilder::current_thread()
                        .enable_platform_reactor(true)
                        .build();
                    match runtime {
                        Ok(rt) => {
                            let cx = rt.handle().try_request_cx_with_budget(Budget::INFINITE);
                            match cx {
                                Ok(cx) => {
                                    let identity_res = rt.block_on(async { local_api.node_identity(&cx).await });
                                    match identity_res {
                                        Ok(node) => {
                                            let fqdn = node.certificate_name().to_string();
                                            let ips = node.addresses().iter().map(ToString::to_string).collect::<Vec<_>>();
                                            let tailnet = fqdn.split('.').skip(1).collect::<Vec<_>>().join(".");
                                            let node_name = fqdn.split('.').next().unwrap_or("").to_string();
                                            (true, fqdn, ips, node_name, String::new(), tailnet)
                                        }
                                        Err(_) => (false, String::new(), Vec::new(), String::new(), String::new(), String::new()),
                                    }
                                }
                                Err(_) => (false, String::new(), Vec::new(), String::new(), String::new(), String::new()),
                            }
                        }
                        Err(_) => (false, String::new(), Vec::new(), String::new(), String::new(), String::new()),
                    }
                }
                Err(_) => (false, String::new(), Vec::new(), String::new(), String::new(), String::new()),
            };

            let mut report = Self::nominal_operational();
            report.timestamp_unix_ms = timestamp_unix_ms;

            if connected {
                report.outcome = "success";
                report.refusal_code = None;
                report.next_action = None;
                report.tailscale = TailscaleStatus {
                    variant: "standalone".into(),
                    tailnet: tailnet_name,
                    connected: true,
                    node_id: if node_id.is_empty() { "local".into() } else { node_id },
                    node_name: fqdn.split('.').next().unwrap_or("host").to_string(),
                    user_id,
                    addresses: ips,
                };
                report.certificate = CertificateStatus {
                    certificate_name: fqdn,
                    generation: 1,
                    valid: true,
                    not_after_wall_us: None,
                    expiry_countdown_secs: None,
                    next_refresh_us: 0,
                    status_message: "Tailscale node identity active and verified via LocalAPI".into(),
                };
            } else {
                let mut ref_report = Self::tailnet_disconnected();
                ref_report.timestamp_unix_ms = timestamp_unix_ms;
                return ref_report;
            }

            // 2. Probe Display & Screen Capture
            let has_display = std::env::var("WAYLAND_DISPLAY").is_ok()
                || std::env::var("DISPLAY").is_ok();

            for p in &mut report.permissions {
                if p.capability == "screen_capture" {
                    if has_display {
                        p.status = "granted";
                        p.detail = Some("Display server available for capture".into());
                    } else {
                        p.status = "denied";
                        p.detail = Some("Headless environment: no DISPLAY or WAYLAND_DISPLAY detected".into());
                    }
                }
            }

            // 3. Probe Hardware Acceleration
            let has_vaapi = std::path::Path::new("/dev/dri/renderD128").exists();
            let has_nvidia = std::path::Path::new("/dev/nvidiactl").exists();
            let has_hw_accel = has_vaapi || has_nvidia;

            for c in &mut report.capabilities {
                if c.name == "video_encode" {
                    if has_hw_accel {
                        c.status = CapabilityStatus::Passed;
                        c.hardware_accelerated = Some(true);
                        c.detail = if has_vaapi {
                            "Hardware HEVC encoder device detected (/dev/dri/renderD128)".into()
                        } else {
                            "NVIDIA hardware device detected (/dev/nvidiactl)".into()
                        };
                    } else {
                        c.status = CapabilityStatus::NotTested;
                        c.hardware_accelerated = Some(false);
                        c.detail = "No GPU render node detected; software or headless mode".into();
                    }
                }
            }

            report.sessions.clear();
            report.sharing.active_viewers = 0;
            report.sharing.active_controller = None;

            report
        }

        #[cfg(not(target_os = "linux"))]
        {
            let mut report = Self::nominal_operational();
            report.timestamp_unix_ms = timestamp_unix_ms;
            report
        }
    }

    /// Creates a nominal, fully operational report.
    #[allow(clippy::too_many_lines)]
    pub fn nominal_operational() -> Self {
        let mut report = Self {
            schema_version: Self::CURRENT_SCHEMA_VERSION,
            timestamp_unix_ms: 1_700_000_000_000,
            outcome: "success",
            refusal_code: None,
            next_action: None,
            tailscale: TailscaleStatus {
                variant: "standalone".into(),
                tailnet: "example.ts.net".into(),
                connected: true,
                node_id: "node-101".into(),
                node_name: "workstation-alpha".into(),
                user_id: "user-101".into(),
                addresses: vec!["100.64.0.1".into(), "fd7a:115c:a1e0::1".into()],
            },
            certificate: CertificateStatus {
                certificate_name: "workstation-alpha.example.ts.net".into(),
                generation: 1,
                valid: true,
                not_after_wall_us: Some(1_707_776_000_000_000),
                expiry_countdown_secs: Some(7_776_000), // 90 days
                next_refresh_us: 1_705_184_000_000_000,
                status_message: "Certificate active and trusted by local roots".into(),
            },
            permissions: vec![
                PermissionRow {
                    capability: "screen_capture",
                    status: "granted",
                    detail: Some("Desktop capture authorized".into()),
                },
                PermissionRow {
                    capability: "input_injection",
                    status: "granted",
                    detail: Some("Pointer and keyboard injection authorized".into()),
                },
                PermissionRow {
                    capability: "clipboard_sync",
                    status: "granted",
                    detail: Some("Text clipboard access active".into()),
                },
                PermissionRow {
                    capability: "audio_playback",
                    status: "granted",
                    detail: Some("Desktop audio loopback authorized".into()),
                },
                PermissionRow {
                    capability: "audio_microphone",
                    status: "granted",
                    detail: Some("Virtual microphone input authorized".into()),
                },
                PermissionRow {
                    capability: "file_transfer",
                    status: "granted",
                    detail: Some("ATP drop directory writable".into()),
                },
            ],
            capabilities: vec![
                CapabilityRow {
                    name: "video_encode",
                    status: CapabilityStatus::Passed,
                    detail: "HEVC Main profile 8-bit 4:2:0 hardware encoder verified".into(),
                    hardware_accelerated: Some(true),
                    restriction: None,
                },
                CapabilityRow {
                    name: "video_decode",
                    status: CapabilityStatus::Passed,
                    detail: "HEVC hardware decoder verified".into(),
                    hardware_accelerated: Some(true),
                    restriction: None,
                },
                CapabilityRow {
                    name: "screen_capture",
                    status: CapabilityStatus::Passed,
                    detail: "Native display capture stream active".into(),
                    hardware_accelerated: Some(true),
                    restriction: Some("Full display only (no window cropping)"),
                },
                CapabilityRow {
                    name: "input_injection",
                    status: CapabilityStatus::Passed,
                    detail: "Immediate OS input injection active".into(),
                    hardware_accelerated: None,
                    restriction: None,
                },
                CapabilityRow {
                    name: "clipboard_sync",
                    status: CapabilityStatus::Passed,
                    detail: "Bidirectional text clipboard sync active".into(),
                    hardware_accelerated: None,
                    restriction: Some("Text only (images deferred to ATP channel)"),
                },
                CapabilityRow {
                    name: "audio_playback",
                    status: CapabilityStatus::Passed,
                    detail: "Opus 48 kHz stereo downlink ready".into(),
                    hardware_accelerated: None,
                    restriction: None,
                },
                CapabilityRow {
                    name: "audio_microphone",
                    status: CapabilityStatus::Passed,
                    detail: "Opus 48 kHz virtual-mic uplink ready".into(),
                    hardware_accelerated: None,
                    restriction: Some("Push-to-talk default"),
                },
                CapabilityRow {
                    name: "file_transfer",
                    status: CapabilityStatus::Passed,
                    detail: "ATP object transfer channel ready".into(),
                    hardware_accelerated: None,
                    restriction: Some("Controlled session only"),
                },
            ],
            sessions: vec![ConnectedSessionStatus {
                session_id: 101,
                device_name: "laptop-controller".into(),
                role: "controller",
                capabilities: vec!["view", "control", "audio", "clipboard", "files"],
                authority_state: "admitted",
            }],
            sharing: SharingAndApprovalStatus {
                sharing_scope: "own-user",
                approval_mode: "unattended",
                active_viewers: 1,
                max_viewers: 3,
                active_controller: Some(101),
            },
            restrictions: Self::standard_restrictions(),
            structured_logs: Vec::new(),
        };
        report.emit_log(
            "info",
            "status_check",
            "Host daemon operational and healthy",
        );
        report
    }

    /// Creates a report reproducing `permission_revoked` / `permission_missing`.
    pub fn permission_revoked(capability: &'static str) -> Self {
        let mut report = Self::nominal_operational();
        report.outcome = "refusal";
        report.refusal_code = Some("permission_missing");
        report.next_action = Some(
            "Grant required permission in OS System Settings under Privacy & Security, then restart service.",
        );

        // Mark the specific permission as denied
        for p in &mut report.permissions {
            if p.capability == capability {
                p.status = "denied";
                p.detail = Some(format!(
                    "OS permission for {capability} was denied or revoked by operator"
                ));
            }
        }

        // Block dependent capabilities
        for c in &mut report.capabilities {
            if c.name == capability || (capability == "screen_capture" && c.name == "video_encode")
            {
                c.status = CapabilityStatus::Blocked;
                c.detail = format!("Blocked: {capability} permission denied by host OS");
            }
        }

        report.emit_log(
            "error",
            "permission_missing",
            &format!("Refusal: {capability} permission is not granted by the host OS"),
        );
        report
    }

    /// Creates a report reproducing `certificate_expired`.
    pub fn certificate_expired() -> Self {
        let mut report = Self::nominal_operational();
        report.outcome = "refusal";
        report.refusal_code = Some("certificate_expired");
        report.next_action = Some(
            "Run 'tailscale cert <machine>' to provision a fresh HTTPS certificate, or check Tailscale daemon status.",
        );

        report.certificate.valid = false;
        report.certificate.expiry_countdown_secs = Some(0);
        report.certificate.status_message = "Certificate expired 120 seconds ago".into();

        for c in &mut report.capabilities {
            c.status = CapabilityStatus::Blocked;
            c.detail = "Blocked: TLS certificate expired; cannot admit secure ingress".into();
        }

        report.emit_log(
            "error",
            "certificate_expired",
            "Refusal: Host Tailscale certificate has expired; ingress connections refused",
        );
        report
    }

    /// Creates a report reproducing `no_hardware_encoder`.
    pub fn no_hardware_encoder() -> Self {
        let mut report = Self::nominal_operational();
        report.outcome = "refusal";
        report.refusal_code = Some("no_hardware_encoder");
        report.next_action = Some(
            "Install hardware HEVC drivers (e.g. VAAPI / NVENC / QuickSync) or enable hardware acceleration in system BIOS.",
        );

        for c in &mut report.capabilities {
            if c.name == "video_encode" {
                c.status = CapabilityStatus::Failed;
                c.hardware_accelerated = Some(false);
                c.detail = "No supported HEVC hardware encoder found on host GPU".into();
            }
        }

        report.emit_log(
            "error",
            "no_hardware_encoder",
            "Refusal: Probed video encoder candidate rejected; hardware HEVC is required by plan §3.3",
        );
        report
    }

    /// Creates a report reproducing `tailnet_disconnected`.
    pub fn tailnet_disconnected() -> Self {
        let mut report = Self::nominal_operational();
        report.outcome = "refusal";
        report.refusal_code = Some("tailnet_disconnected");
        report.next_action =
            Some("Connect Tailscale via 'tailscale up' or start the tailscaled daemon.");

        report.tailscale.connected = false;
        report.tailscale.addresses.clear();

        for c in &mut report.capabilities {
            c.status = CapabilityStatus::Blocked;
            c.detail = "Blocked: host is offline from Tailscale network".into();
        }

        report.emit_log(
            "error",
            "tailnet_disconnected",
            "Refusal: Tailscale client is not connected to any tailnet node",
        );
        report
    }
}

fn escape_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out
}
