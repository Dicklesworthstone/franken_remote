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
            let _ = write!(
                rows,
                "{{\"node_id\":{},\"certificate_name\":{},\"addresses\":[{}],\"desktop_available\":null,\"access_authorized\":null}}",
                quoted(peer.stable_id()),
                quoted(peer.certificate_name()),
                ips
            );
        } else {
            // Escaped text prevents local terminal controls/bidi in opaque IDs.
            let _ = writeln!(
                rows,
                "{}  {}  desktop: not probed",
                peer.stable_id().escape_default(),
                peer.certificate_name().escape_default()
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

        format!(
            "{{\"schema_version\":1,\"timestamp_unix_ms\":{},\"outcome\":\"success\",\"node\":{{\"certificate_name\":{},\"addresses\":[{}]}},\"port\":{},\"port_collisions\":[{}],\"honest_endpoints\":{{\"https\":{},\"quic\":{}}},\"alpn_protocols\":[{}],\"certificate_transparency_notice\":{},\"certificate\":{}}}\n",
            timestamp(),
            quoted(&report.certificate_name),
            addresses_json,
            report.port,
            collisions_json,
            quoted(&report.https_endpoint),
            quoted(&report.quic_endpoint),
            alpn_json,
            quoted(report.certificate_transparency_notice),
            cert_json
        )
    } else {
        let mut out = String::new();
        let _ = writeln!(out, "FrankenRemote Host Diagnosis:");
        let _ = writeln!(out, "  Node Certificate Name: {}", report.certificate_name);
        let _ = writeln!(out, "  Addresses: {}", report.addresses.join(", "));
        if report.port_collisions.is_empty() {
            let _ = writeln!(
                out,
                "  Service Port: {} (available, no collisions)",
                report.port
            );
        } else {
            let collisions = report
                .port_collisions
                .iter()
                .map(|c| format!("{}:{} ({})", c.ip, c.port, c.protocol))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                out,
                "  Service Port: {} (COLLISION DETECTED on {})",
                report.port, collisions
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
            "  Certificate Transparency Notice: {}",
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
                        "    Expiry Countdown: {}s ({}h {}m {}s)",
                        countdown, hours, mins, secs
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
        };

        let json = doctor(&report, true);
        assert!(json.contains("\"schema_version\":1"));
        assert!(json.contains("\"outcome\":\"success\""));
        assert!(json.contains("\"certificate_name\":\"node.tailnet.ts.net\""));
        assert!(json.contains("\"expiry_countdown_secs\":86400"));
        assert!(json.contains("\"kind\":\"Rotated\""));
        assert!(json.contains("\"https\":\"https://node.tailnet.ts.net:8443/\""));

        let text = doctor(&report, false);
        assert!(text.contains("Node Certificate Name: node.tailnet.ts.net"));
        assert!(text.contains("Expiry Countdown: 86400s (24h 0m 0s)"));
        assert!(text.contains("Last Event: Rotated [gen 2]"));
        assert!(text.contains("Service Port: 8443 (available, no collisions)"));
    }
}
