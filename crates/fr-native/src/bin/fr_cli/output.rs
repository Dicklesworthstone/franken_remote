use super::Failure;
use std::{
    fmt::Write as _,
    io::{self, Write},
    time::{SystemTime, UNIX_EPOCH},
};

/// Only bounded local command output uses this encoder. No protocol decoder,
/// serializer dependency, screen, clipboard, certificate or input text is added.
pub fn quoted(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c <= '\u{1f}' => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
pub fn timestamp() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_millis()).ok())
        .map_or_else(|| "null".into(), |n| n.to_string())
}
pub fn failure(error: Failure, json: bool) -> String {
    if json {
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":{},\"error\":{{\"code\":{},\"next_action\":{}}}}}\n",
            timestamp(),
            quoted(if error.exit == 130 {
                "cancelled"
            } else {
                "refused"
            }),
            quoted(error.code),
            quoted(error.next)
        )
    } else {
        format!("{}: {}\n", error.code, error.next)
    }
}
pub fn write(text: &str) -> io::Result<()> {
    // The producer has finished; slow stdout never runs inside session callbacks.
    if text.len() > 2 * 1024 * 1024 {
        return Err(io::Error::other("output bound"));
    }
    let mut out = io::stdout().lock();
    out.write_all(text.as_bytes())?;
    out.flush()
}
#[cfg(target_os = "linux")]
pub fn hosts(snapshot: &frd::native_connection::Discovery, json: bool) -> String {
    let excluded = snapshot.excluded();
    let mut rows = String::new();
    for (i, peer) in snapshot.peers().iter().enumerate() {
        if json {
            if i != 0 {
                rows.push(',');
            }
            let ips = peer
                .addresses()
                .iter()
                .map(|a| quoted(&a.to_string()))
                .collect::<Vec<_>>()
                .join(",");
            let path_info = match peer.transport_path() {
                fr_tailnet::PeerTransportPath::Direct { cur_addr } => {
                    format!(
                        ",\"transport_path\":{{\"kind\":\"direct\",\"addr\":{}}}",
                        quoted(cur_addr)
                    )
                }
                fr_tailnet::PeerTransportPath::DerpRelayed { relay } => {
                    format!(
                        ",\"transport_path\":{{\"kind\":\"derp_relayed\",\"relay\":{}}},\"warning\":\"tailnet_direct_path_unavailable\"",
                        quoted(relay)
                    )
                }
                fr_tailnet::PeerTransportPath::Unknown => ",\"transport_path\":null".to_string(),
            };
            let _ = write!(
                rows,
                "{{\"node_id\":{},\"certificate_name\":{},\"addresses\":[{}],\"desktop_available\":null,\"access_authorized\":null{}}}",
                quoted(peer.stable_id()),
                quoted(peer.certificate_name()),
                ips,
                path_info
            );
        } else {
            let path_badge = match peer.transport_path() {
                fr_tailnet::PeerTransportPath::Direct { cur_addr } => {
                    format!(" [direct: {cur_addr}]")
                }
                fr_tailnet::PeerTransportPath::DerpRelayed { relay } => {
                    format!(" [DERP RELAY: {relay} - LATENCY ELEVATED]")
                }
                fr_tailnet::PeerTransportPath::Unknown => String::new(),
            };
            // Escaped text prevents local terminal controls/bidi in opaque IDs.
            let _ = writeln!(
                rows,
                "{}  {}  desktop: not probed{}",
                peer.stable_id().escape_default(),
                peer.certificate_name().escape_default(),
                path_badge
            );
        }
    }
    if json {
        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"success\",\"snapshot_only\":true,\"peers\":[{}],\"excluded\":{{\"shared\":{},\"expired\":{},\"unusable\":{}}}}}\n",
            timestamp(),
            rows,
            excluded.shared,
            excluded.expired,
            excluded.unusable
        )
    } else {
        format!(
            "Machines from installed Tailscale (desktop availability and access are unknown):\n{}Excluded: {} shared, {} expired, {} unusable.\n",
            rows, excluded.shared, excluded.expired, excluded.unusable
        )
    }
}

#[derive(Debug, Clone)]
pub struct DoctorReport {
    pub certificate_name: String,
    pub addresses: Vec<String>,
    pub port: u16,
    pub port_collisions: Vec<CollisionReport>,
    pub https_endpoint: String,
    pub quic_endpoint: String,
    pub alpn_protocols: Vec<String>,
    pub certificate_transparency_notice: &'static str,
    pub certificate: Option<CertificateReport>,
    pub permissions: Vec<DoctorPermissionReport>,
    pub capabilities: Vec<DoctorCapabilityReport>,
    pub sessions: Vec<DoctorSessionReport>,
    pub sharing_scope: &'static str,
    pub approval_mode: &'static str,
    pub restrictions: Vec<DoctorRestrictionReport>,
    pub refusal_code: Option<&'static str>,
    pub next_action: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorPermissionReport {
    pub capability: &'static str,
    pub status: &'static str, // "granted", "denied", "prompt_needed", "not_tested", "unsupported"
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorCapabilityReport {
    pub name: &'static str,
    pub status: &'static str, // "passed", "failed", "blocked", "not_tested"
    pub detail: String,
    pub hardware_accelerated: Option<bool>,
    pub restriction: Option<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorSessionReport {
    pub session_id: u64,
    pub device_name: String,
    pub role: &'static str,
    pub capabilities: Vec<&'static str>,
    pub authority_state: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorRestrictionReport {
    pub code: &'static str,
    pub summary: &'static str,
    pub scope: &'static str,
}

impl DoctorReport {
    /// `fr doctor` probes identity, ports and (on request) the certificate
    /// only; every OS permission row stays `not_tested` until a probe fills it.
    pub fn unmeasured_permissions() -> Vec<DoctorPermissionReport> {
        [
            "screen_capture",
            "input_injection",
            "clipboard_sync",
            "audio_playback",
            "audio_microphone",
            "file_transfer",
        ]
        .into_iter()
        .map(|capability| DoctorPermissionReport {
            capability,
            status: "not_tested",
            detail: Some("Not probed by fr doctor".into()),
        })
        .collect()
    }

    /// No media, input, clipboard, audio or transfer probe runs in `fr doctor`.
    pub fn unmeasured_capabilities() -> Vec<DoctorCapabilityReport> {
        [
            "video_encode",
            "video_decode",
            "screen_capture",
            "input_injection",
            "clipboard_sync",
            "audio_playback",
            "audio_microphone",
            "file_transfer",
        ]
        .into_iter()
        .map(|name| DoctorCapabilityReport {
            name,
            status: "not_tested",
            detail: "Not probed by fr doctor".into(),
            hardware_accelerated: None,
            restriction: None,
        })
        .collect()
    }

    pub fn standard_restrictions() -> Vec<DoctorRestrictionReport> {
        vec![
            DoctorRestrictionReport {
                code: "video_hevc_only",
                summary: "HEVC (Main, 8-bit, 4:2:0 baseline) only. No secondary video codec.",
                scope: "media",
            },
            DoctorRestrictionReport {
                code: "audio_opus_only",
                summary: "Opus audio only (48 kHz mono/stereo). No alternative audio codec.",
                scope: "audio",
            },
            DoctorRestrictionReport {
                code: "full_display_only",
                summary: "Selected full-display capture only. Window cropping deferred.",
                scope: "capture",
            },
            DoctorRestrictionReport {
                code: "tailscale_ingress_only",
                summary: "Tailscale authenticated ingress only. No public relays or pairing PINs.",
                scope: "network",
            },
            DoctorRestrictionReport {
                code: "single_controller_authority",
                summary: "Single controller owns input authority at a time; up to 2 read-only observers.",
                scope: "input",
            },
            DoctorRestrictionReport {
                code: "no_zero_rtt_application_data",
                summary: "Zero-RTT application data is forbidden; requires full TLS/QUIC handshake.",
                scope: "security",
            },
        ]
    }
}

#[derive(Debug, Clone)]
pub struct CollisionReport {
    pub ip: String,
    pub port: u16,
    pub protocol: &'static str,
}

#[derive(Debug, Clone)]
pub struct CertificateReport {
    pub generation: u64,
    pub not_after_wall_us: Option<u64>,
    pub expiry_countdown_secs: Option<u64>,
    pub next_refresh_us: u64,
    pub last_event: Option<EventReport>,
    pub recent_events: Vec<EventReport>,
}

#[derive(Debug, Clone)]
pub struct EventReport {
    pub kind: &'static str,
    pub generation: u64,
    pub timestamp_wall_us: u64,
    pub reason: Option<String>,
}

#[allow(clippy::too_many_lines)]
pub fn doctor(report: &DoctorReport, json: bool) -> String {
    if json {
        let addresses_json = report
            .addresses
            .iter()
            .map(|a| quoted(a))
            .collect::<Vec<_>>()
            .join(",");
        let collisions_json = report
            .port_collisions
            .iter()
            .map(|c| {
                format!(
                    "{{\"ip\":{},\"port\":{},\"protocol\":{}}}",
                    quoted(&c.ip),
                    c.port,
                    quoted(c.protocol)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let alpn_json = report
            .alpn_protocols
            .iter()
            .map(|p| quoted(p))
            .collect::<Vec<_>>()
            .join(",");
        let cert_json = if let Some(cert) = &report.certificate {
            let last_event_json = if let Some(ev) = &cert.last_event {
                format!(
                    "{{\"kind\":{},\"generation\":{},\"timestamp_wall_us\":{},\"reason\":{}}}",
                    quoted(ev.kind),
                    ev.generation,
                    ev.timestamp_wall_us,
                    ev.reason
                        .as_ref()
                        .map_or_else(|| "null".into(), |r| quoted(r))
                )
            } else {
                "null".into()
            };
            let recent_events_json = cert
                .recent_events
                .iter()
                .map(|ev| {
                    format!(
                        "{{\"kind\":{},\"generation\":{},\"timestamp_wall_us\":{},\"reason\":{}}}",
                        quoted(ev.kind),
                        ev.generation,
                        ev.timestamp_wall_us,
                        ev.reason
                            .as_ref()
                            .map_or_else(|| "null".into(), |r| quoted(r))
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!(
                "{{\"generation\":{},\"not_after_wall_us\":{},\"expiry_countdown_secs\":{},\"next_refresh_us\":{},\"last_event\":{},\"recent_events\":[{}]}}",
                cert.generation,
                cert.not_after_wall_us
                    .map_or_else(|| "null".into(), |v| v.to_string()),
                cert.expiry_countdown_secs
                    .map_or_else(|| "null".into(), |v| v.to_string()),
                cert.next_refresh_us,
                last_event_json,
                recent_events_json
            )
        } else {
            "null".into()
        };

        let permissions_json = report
            .permissions
            .iter()
            .map(|p| {
                let detail_str = p
                    .detail
                    .as_ref()
                    .map_or_else(|| "null".into(), |d| quoted(d));
                format!(
                    "{{\"capability\":{},\"status\":{},\"detail\":{}}}",
                    quoted(p.capability),
                    quoted(p.status),
                    detail_str
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        let capabilities_json = report
            .capabilities
            .iter()
            .map(|c| {
                let hw_str = c
                    .hardware_accelerated
                    .map_or_else(|| "null".into(), |h| h.to_string());
                let restr_str = c.restriction.map_or_else(|| "null".into(), quoted);
                format!(
                    "{{\"name\":{},\"status\":{},\"detail\":{},\"hardware_accelerated\":{},\"restriction\":{}}}",
                    quoted(c.name),
                    quoted(c.status),
                    quoted(&c.detail),
                    hw_str,
                    restr_str
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        let sessions_json = report
            .sessions
            .iter()
            .map(|s| {
                let caps = s
                    .capabilities
                    .iter()
                    .map(|c| quoted(c))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    "{{\"session_id\":{},\"device_name\":{},\"role\":{},\"capabilities\":[{}],\"authority_state\":{}}}",
                    s.session_id,
                    quoted(&s.device_name),
                    quoted(s.role),
                    caps,
                    quoted(s.authority_state)
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        let restrictions_json = report
            .restrictions
            .iter()
            .map(|r| {
                format!(
                    "{{\"code\":{},\"summary\":{},\"scope\":{}}}",
                    quoted(r.code),
                    quoted(r.summary),
                    quoted(r.scope)
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        let outcome = if report.refusal_code.is_some() {
            "refusal"
        } else {
            "success"
        };
        let refusal_code_json = report.refusal_code.map_or_else(|| "null".into(), quoted);
        let next_action_json = report.next_action.map_or_else(|| "null".into(), quoted);

        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":{},\"node\":{{\"certificate_name\":{},\"addresses\":[{}]}},\"port\":{},\"port_collisions\":[{}],\"honest_endpoints\":{{\"https\":{},\"quic\":{}}},\"alpn_protocols\":[{}],\"certificate_transparency_notice\":{},\"certificate\":{},\"permissions\":[{}],\"capabilities\":[{}],\"sessions\":[{}],\"sharing\":{{\"sharing_scope\":{},\"approval_mode\":{}}},\"restrictions\":[{}],\"refusal_code\":{},\"next_action\":{}}}\n",
            timestamp(),
            quoted(outcome),
            quoted(&report.certificate_name),
            addresses_json,
            report.port,
            collisions_json,
            quoted(&report.https_endpoint),
            quoted(&report.quic_endpoint),
            alpn_json,
            quoted(report.certificate_transparency_notice),
            cert_json,
            permissions_json,
            capabilities_json,
            sessions_json,
            quoted(report.sharing_scope),
            quoted(report.approval_mode),
            restrictions_json,
            refusal_code_json,
            next_action_json
        )
    } else {
        let mut out = String::new();
        let _ = writeln!(out, "FrankenRemote Host Doctor Report");
        let _ = writeln!(out, "  Node Certificate Name: {}", report.certificate_name);
        let _ = writeln!(
            out,
            "  Tailnet IP Addresses: {}",
            report.addresses.join(", ")
        );
        let _ = writeln!(
            out,
            "  Service Port: {}{}",
            report.port,
            if report.port_collisions.is_empty() {
                " (available, no collisions)"
            } else {
                " (PORT COLLISION DETECTED)"
            }
        );
        for col in &report.port_collisions {
            let _ = writeln!(
                out,
                "    WARNING: Port collision on {}:{} ({})",
                col.ip, col.port, col.protocol
            );
        }
        let _ = writeln!(out, "  Honest Endpoints:");
        let _ = writeln!(out, "    HTTPS: {}", report.https_endpoint);
        let _ = writeln!(out, "    QUIC:  {}", report.quic_endpoint);
        let _ = writeln!(
            out,
            "  ALPN Protocols: {}",
            report.alpn_protocols.join(", ")
        );
        let _ = writeln!(
            out,
            "  Certificate Transparency Notice:\n    {}",
            report.certificate_transparency_notice
        );
        match &report.certificate {
            Some(cert) => {
                let _ = writeln!(out, "  Certificate Status:");
                let _ = writeln!(out, "    Active Generation: {}", cert.generation);
                if let Some(countdown) = cert.expiry_countdown_secs {
                    let hours = countdown / 3600;
                    let mins = (countdown % 3600) / 60;
                    let secs = countdown % 60;
                    let _ = writeln!(
                        out,
                        "    Expiry Countdown: {countdown}s ({hours}h {mins}m {secs}s)"
                    );
                } else {
                    let _ = writeln!(out, "    Expiry Countdown: unknown");
                }
                if let Some(last) = &cert.last_event {
                    let reason_str = last.reason.as_deref().unwrap_or("");
                    let reason_disp = if reason_str.is_empty() {
                        String::new()
                    } else {
                        format!(" (reason: {reason_str})")
                    };
                    let _ = writeln!(
                        out,
                        "    Last Event: {} [gen {}]{}",
                        last.kind, last.generation, reason_disp
                    );
                }
                if !cert.recent_events.is_empty() {
                    let _ = writeln!(out, "    Recent Events ({}):", cert.recent_events.len());
                    for ev in &cert.recent_events {
                        let reason_str = ev.reason.as_deref().unwrap_or("");
                        let reason_disp = if reason_str.is_empty() {
                            String::new()
                        } else {
                            format!(" (reason: {reason_str})")
                        };
                        let _ = writeln!(
                            out,
                            "      - [gen {}] {}{}",
                            ev.generation, ev.kind, reason_disp
                        );
                    }
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "  Certificate Status: not probed (pass --trust-roots to verify TLS 1.3 certificate status and rotation events)"
                );
            }
        }
        let _ = writeln!(out, "  OS Permissions:");
        for p in &report.permissions {
            let detail_text = p.detail.as_deref().unwrap_or("");
            let badge = match p.status {
                "granted" => "[GRANTED]       ",
                "denied" => "[DENIED]        ",
                "prompt_needed" => "[PROMPT NEEDED] ",
                "not_tested" => "[NOT TESTED]    ",
                _ => "[UNSUPPORTED]   ",
            };
            let _ = writeln!(out, "    {:18} {} {}", p.capability, badge, detail_text);
        }
        let _ = writeln!(out, "  Capability Matrix:");
        for c in &report.capabilities {
            let badge = match c.status {
                "passed" => "[PASSED]    ",
                "failed" => "[FAILED]    ",
                "blocked" => "[BLOCKED]   ",
                _ => "[NOT TESTED]",
            };
            let hw = match c.hardware_accelerated {
                Some(true) => " [HW]",
                Some(false) => " [SW]",
                None => "",
            };
            let _ = writeln!(out, "    {:18} {}{} : {}", c.name, badge, hw, c.detail);
            if let Some(r) = c.restriction {
                let _ = writeln!(out, "      * Restriction: {r}");
            }
        }
        let _ = writeln!(
            out,
            "  Sharing & Admission: scope={} approval={}",
            report.sharing_scope, report.approval_mode
        );
        if report.sessions.is_empty() {
            let _ = writeln!(out, "  Connected Sessions: None");
        } else {
            let _ = writeln!(out, "  Connected Sessions ({}):", report.sessions.len());
            for s in &report.sessions {
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
        let _ = writeln!(out, "  Known Restrictions:");
        for r in &report.restrictions {
            let _ = writeln!(out, "    * [{}] {}: {}", r.scope, r.code, r.summary);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn json_escaping_preserves_unicode_and_escapes_every_control_and_delimiter() {
        assert_eq!(
            quoted("a\"\\\n\t\0é"),
            "\"a\\\"\\\\\\u000a\\u0009\\u0000é\""
        );
        for c in '\0'..='\u{1f}' {
            assert!(!quoted(&c.to_string()).contains(c));
        }
    }
    #[test]
    fn failures_are_versioned_and_never_echo_untrusted_arguments() {
        let s = failure(
            Failure::new("cancelled", "Inspect cleanup before starting again.", 130),
            true,
        );
        assert!(s.contains("\"schema_version\":1"));
        assert!(s.contains("\"outcome\":\"cancelled\""));
        assert!(s.ends_with('\n'));
    }
    #[test]
    fn doctor_formats_honest_endpoints_and_events_in_json_and_text() {
        let report = DoctorReport {
            certificate_name: "node.tailnet.ts.net".into(),
            addresses: vec!["100.64.0.1".into()],
            port: 8443,
            port_collisions: vec![],
            https_endpoint: "https://node.tailnet.ts.net:8443/".into(),
            quic_endpoint: "quic://node.tailnet.ts.net:8443".into(),
            alpn_protocols: vec!["fr-remote/0".into(), "h3".into()],
            certificate_transparency_notice: "CT notice",
            certificate: Some(CertificateReport {
                generation: 2,
                not_after_wall_us: Some(1_700_000_000_000_000),
                expiry_countdown_secs: Some(86400),
                next_refresh_us: 1_699_999_000_000_000,
                last_event: Some(EventReport {
                    kind: "Rotated",
                    generation: 2,
                    timestamp_wall_us: 1_699_900_000_000_000,
                    reason: None,
                }),
                recent_events: vec![
                    EventReport {
                        kind: "Provisioned",
                        generation: 1,
                        timestamp_wall_us: 1_699_800_000_000_000,
                        reason: None,
                    },
                    EventReport {
                        kind: "Rotated",
                        generation: 2,
                        timestamp_wall_us: 1_699_900_000_000_000,
                        reason: None,
                    },
                ],
            }),
            permissions: DoctorReport::unmeasured_permissions(),
            capabilities: DoctorReport::unmeasured_capabilities(),
            sessions: vec![],
            sharing_scope: "own-user",
            approval_mode: "unattended",
            restrictions: DoctorReport::standard_restrictions(),
            refusal_code: None,
            next_action: None,
        };

        let json = doctor(&report, true);
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"outcome\":\"success\""));
        assert!(json.contains("\"certificate_name\":\"node.tailnet.ts.net\""));
        assert!(json.contains("\"expiry_countdown_secs\":86400"));
        assert!(json.contains("\"kind\":\"Rotated\""));
        assert!(json.contains("\"https\":\"https://node.tailnet.ts.net:8443/\""));
        assert!(json.contains("\"permissions\":["));
        assert!(json.contains("\"capabilities\":["));
        assert!(json.contains("\"restrictions\":["));
        assert!(json.contains("\"sharing_scope\":\"own-user\""));
        assert!(!json.contains("\"passed\"") && !json.contains("\"granted\""));
        assert!(json.contains("\"status\":\"not_tested\""));

        let text = doctor(&report, false);
        assert!(text.contains("Node Certificate Name: node.tailnet.ts.net"));
        assert!(text.contains("Expiry Countdown: 86400s (24h 0m 0s)"));
        assert!(text.contains("Last Event: Rotated [gen 2]"));
        assert!(text.contains("Service Port: 8443 (available, no collisions)"));
        assert!(text.contains("OS Permissions:"));
        assert!(text.contains("screen_capture     [NOT TESTED]"));
        assert!(!text.contains("[PASSED]") && !text.contains("[GRANTED]"));
        assert!(text.contains("Capability Matrix:"));
        assert!(text.contains("Known Restrictions:"));
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn hosts_formats_direct_and_derp_relay_in_json_and_text() {
        let peer_direct = fr_tailnet::DiscoveredPeer::from_parts(
            "node-1".into(),
            "peer1.ts.net".into(),
            vec!["100.64.0.2".parse().unwrap()],
            fr_tailnet::PeerTransportPath::Direct {
                cur_addr: "192.168.1.100:41641".into(),
            },
        );
        let peer_derp = fr_tailnet::DiscoveredPeer::from_parts(
            "node-2".into(),
            "peer2.ts.net".into(),
            vec!["100.64.0.3".parse().unwrap()],
            fr_tailnet::PeerTransportPath::DerpRelayed {
                relay: "sfo".into(),
            },
        );
        let discovery = fr_tailnet::Discovery::from_parts(
            vec![peer_direct, peer_derp],
            fr_tailnet::DiscoveryExclusions::default(),
            1_000,
            2_000,
        );

        let json = hosts(&discovery, true);
        assert!(json.contains("\"schema_version\":1"));
        assert!(
            json.contains(
                "\"transport_path\":{\"kind\":\"direct\",\"addr\":\"192.168.1.100:41641\"}"
            )
        );
        assert!(json.contains("\"transport_path\":{\"kind\":\"derp_relayed\",\"relay\":\"sfo\"}"));
        assert!(json.contains("\"warning\":\"tailnet_direct_path_unavailable\""));

        let text = hosts(&discovery, false);
        assert!(text.contains("[direct: 192.168.1.100:41641]"));
        assert!(text.contains("[DERP RELAY: sfo - LATENCY ELEVATED]"));
    }
}
