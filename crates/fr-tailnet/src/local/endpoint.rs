//! Configurable service port, collision detection, and honest endpoint reporting.
//! Default port is unprivileged 8443 for TCP and UDP.
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};

pub const DEFAULT_SERVICE_PORT: u16 = 8443;
pub const PROJECT_ALPN: &[u8] = b"fr-remote/0";
pub const WEBTRANSPORT_ALPN: &[u8] = b"h3";

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TransportProtocol {
    Tcp,
    Udp,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PortCollision {
    pub ip: IpAddr,
    pub port: u16,
    pub protocol: TransportProtocol,
}

impl std::fmt::Display for PortCollision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "port collision on {}:{} ({:?})",
            self.ip, self.port, self.protocol
        )
    }
}

impl std::error::Error for PortCollision {}

/// Detect port collision on the specified IP and port for both TCP and UDP.
/// Binds test sockets to verify availability and drops them immediately.
/// Never overwrites an existing listener or Tailscale Serve configuration.
pub fn check_port_collision(ip: IpAddr, port: u16) -> Result<(), PortCollision> {
    let addr = SocketAddr::new(ip, port);
    // Check TCP
    match TcpListener::bind(addr) {
        Ok(listener) => {
            drop(listener);
        }
        Err(_) => {
            return Err(PortCollision {
                ip,
                port,
                protocol: TransportProtocol::Tcp,
            });
        }
    }
    // Check UDP
    match UdpSocket::bind(addr) {
        Ok(socket) => {
            drop(socket);
        }
        Err(_) => {
            return Err(PortCollision {
                ip,
                port,
                protocol: TransportProtocol::Udp,
            });
        }
    }
    Ok(())
}

/// Formats the honest HTTPS / WebTransport endpoint for the host FQDN.
pub fn honest_https_endpoint(fqdn: &str, port: u16) -> String {
    let clean_fqdn = fqdn.trim_end_matches('.');
    if port == 443 {
        format!("https://{clean_fqdn}")
    } else {
        format!("https://{clean_fqdn}:{port}")
    }
}

/// Formats the honest native QUIC endpoint.
pub fn honest_quic_endpoint(fqdn: &str, port: u16) -> String {
    let clean_fqdn = fqdn.trim_end_matches('.');
    format!("quic://{clean_fqdn}:{port}")
}

/// Notice explaining Certificate Transparency (CT) visibility of the machine name.
pub const CERTIFICATE_TRANSPARENCY_NOTICE: &str = "Tailscale HTTPS certificates are issued via Let's Encrypt and published to public \
     Certificate Transparency logs. The machine name and tailnet domain are publicly visible \
     in CT logs. See https://tailscale.com/kb/1153/enabling-https";

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, TcpListener, UdpSocket};

    #[test]
    fn honest_endpoints_format_correctly() {
        assert_eq!(
            honest_https_endpoint("box.my-tailnet.ts.net.", 8443),
            "https://box.my-tailnet.ts.net:8443"
        );
        assert_eq!(
            honest_https_endpoint("box.my-tailnet.ts.net", 443),
            "https://box.my-tailnet.ts.net"
        );
        assert_eq!(
            honest_quic_endpoint("box.my-tailnet.ts.net.", 8443),
            "quic://box.my-tailnet.ts.net:8443"
        );
    }

    #[test]
    fn port_collision_detects_bound_tcp_and_udp() {
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        // Find an unused ephemeral port
        let probe = TcpListener::bind(SocketAddr::new(ip, 0)).unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);

        // Port is free: should succeed
        assert!(check_port_collision(ip, port).is_ok());

        // Bind TCP on that port: should detect collision
        let tcp = TcpListener::bind(SocketAddr::new(ip, port)).unwrap();
        let collision = check_port_collision(ip, port).unwrap_err();
        assert_eq!(collision.protocol, TransportProtocol::Tcp);
        assert_eq!(collision.port, port);
        drop(tcp);

        // Bind UDP on that port: should detect collision
        let udp = UdpSocket::bind(SocketAddr::new(ip, port)).unwrap();
        let collision = check_port_collision(ip, port).unwrap_err();
        assert_eq!(collision.protocol, TransportProtocol::Udp);
        assert_eq!(collision.port, port);
        drop(udp);

        // Freed again: should succeed
        assert!(check_port_collision(ip, port).is_ok());
    }
}
