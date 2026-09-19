#![forbid(unsafe_code)]
//! Host doctor inspection: verifies installed Tailscale status, checks port
//! collision on the configured service port, generates honest endpoints,
//! surfaces certificate transparency notice, and queries TLS 1.3 certificate status
//! and recent lifecycle events with expiry countdown.
use super::{
    Cell, Cx, DoctorOptions, Failure, LocalApi, Runtime, Shutdown, output, read_roots, tailnet,
};
use asupersync::tls::RootCertStore;
use fr_tailnet::{
    CERTIFICATE_TRANSPARENCY_NOTICE, CertificateEventKind, CertificatePolicy, PROJECT_ALPN,
    WEBTRANSPORT_ALPN, check_port_collision, honest_https_endpoint, honest_quic_endpoint,
};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn run(
    runtime: &Runtime,
    cx: &Cx,
    shutdown: &mut Shutdown,
    stopped: &Cell<bool>,
    api: LocalApi,
    doctor: &DoctorOptions,
    json: bool,
) -> Result<String, Failure> {
    // 1. Query node identity from LocalApi.
    let node_op = api.node_identity(cx);
    let node = runtime
        .block_on(shutdown.run(cx, stopped, node_op))
        .map_err(tailnet)?;

    if stopped.get() {
        return Err(Failure::new(
            "cancelled",
            "Doctor inspection was stopped; no actions were replayed.",
            130,
        ));
    }

    let cert_name = node.certificate_name().to_string();
    let addresses = node
        .addresses()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();

    // 2. Check port collisions on all tailnet IPs for configured port.
    let mut port_collisions = Vec::new();
    for ip in node.addresses() {
        if let Err(collision) = check_port_collision(*ip, doctor.port) {
            port_collisions.push(output::CollisionReport {
                ip: ip.to_string(),
                port: doctor.port,
                protocol: match collision.protocol {
                    fr_tailnet::TransportProtocol::Tcp => "tcp",
                    fr_tailnet::TransportProtocol::Udp => "udp",
                },
            });
        }
    }

    // 3. Formulate honest endpoints.
    let https_endpoint = honest_https_endpoint(&cert_name, doctor.port);
    let quic_endpoint = honest_quic_endpoint(&cert_name, doctor.port);

    // 4. Formulate ALPN protocols.
    let alpn_protocols = vec![
        std::str::from_utf8(PROJECT_ALPN)
            .unwrap_or("fr-remote/0")
            .to_string(),
        std::str::from_utf8(WEBTRANSPORT_ALPN)
            .unwrap_or("h3")
            .to_string(),
    ];

    // 5. Query TLS 1.3 certificate status if trust roots are provided.
    let certificate = if let Some(roots_path) = &doctor.roots {
        let certs = read_roots(roots_path)?;
        let mut store = RootCertStore::empty();
        for cert in certs {
            let _ = store.add(&cert);
        }
        let policy = CertificatePolicy::default();
        let server_id_op = api.native_server_identity(cx, store, policy);
        let server_id = runtime
            .block_on(shutdown.run(cx, stopped, server_id_op))
            .map_err(tailnet)?;

        if stopped.get() {
            return Err(Failure::new(
                "cancelled",
                "Doctor inspection was stopped; no actions were replayed.",
                130,
            ));
        }

        let status = server_id.status(cx).map_err(tailnet)?;
        let events = server_id.events().map_err(tailnet)?;

        let now_wall_us = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_micros()).ok())
            .unwrap_or(0);

        let expiry_countdown_secs = status.expiry_countdown_secs(now_wall_us);

        let last_event = status.last_event.as_ref().map(|ev| output::EventReport {
            kind: event_kind_str(ev.kind),
            generation: ev.generation,
            timestamp_wall_us: ev.timestamp_wall_us,
            reason: ev.reason.map(|r| format!("{r:?}")),
        });

        let recent_events = events
            .iter()
            .map(|ev| output::EventReport {
                kind: event_kind_str(ev.kind),
                generation: ev.generation,
                timestamp_wall_us: ev.timestamp_wall_us,
                reason: ev.reason.map(|r| format!("{r:?}")),
            })
            .collect();

        Some(output::CertificateReport {
            generation: status.generation,
            not_after_wall_us: status.not_after_wall_us,
            expiry_countdown_secs,
            next_refresh_us: status.next_refresh_us,
            last_event,
            recent_events,
        })
    } else {
        None
    };

    let report = output::DoctorReport {
        certificate_name: cert_name,
        addresses,
        port: doctor.port,
        port_collisions,
        https_endpoint,
        quic_endpoint,
        alpn_protocols,
        certificate_transparency_notice: CERTIFICATE_TRANSPARENCY_NOTICE,
        certificate,
    };

    Ok(output::doctor(&report, json))
}

fn event_kind_str(kind: CertificateEventKind) -> &'static str {
    match kind {
        CertificateEventKind::Provisioned => "Provisioned",
        CertificateEventKind::Rotated => "Rotated",
        CertificateEventKind::RenewalFailed => "RenewalFailed",
        CertificateEventKind::Expired => "Expired",
        CertificateEventKind::IdentityChanged => "IdentityChanged",
        CertificateEventKind::Stopped => "Stopped",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_kind_strings_match_variants() {
        assert_eq!(
            event_kind_str(CertificateEventKind::Provisioned),
            "Provisioned"
        );
        assert_eq!(event_kind_str(CertificateEventKind::Rotated), "Rotated");
        assert_eq!(
            event_kind_str(CertificateEventKind::RenewalFailed),
            "RenewalFailed"
        );
        assert_eq!(event_kind_str(CertificateEventKind::Expired), "Expired");
        assert_eq!(
            event_kind_str(CertificateEventKind::IdentityChanged),
            "IdentityChanged"
        );
        assert_eq!(event_kind_str(CertificateEventKind::Stopped), "Stopped");
    }

    #[test]
    fn port_collision_and_honest_endpoints_format_reliably() {
        let fqdn = "workstation.fixture.ts.net";
        let port = 8443;
        let https = honest_https_endpoint(fqdn, port);
        let quic = honest_quic_endpoint(fqdn, port);
        assert_eq!(https, "https://workstation.fixture.ts.net:8443/");
        assert_eq!(quic, "quic://workstation.fixture.ts.net:8443");
        assert_eq!(
            CERTIFICATE_TRANSPARENCY_NOTICE,
            "Tailscale HTTPS certificates are logged in public Certificate Transparency logs, which publishes the machine's tailnet FQDN."
        );
    }
}
