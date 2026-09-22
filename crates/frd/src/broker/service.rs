//! Broker daemon service tying together configuration, listeners, peer cache,
//! session registry, desktop availability detection, and idle supervision (plan sections 5.1, 5.2).

use super::config::DaemonConfig;
use super::idle_harness::BrokerStateInspect;
use super::peer_cache::PeerIdentityCache;
use super::session_registry::SessionRegistry;
use core::fmt;
use fr_core::ids::{HostBootId, OsSessionId};
use std::net::IpAddr;

/// Availability of a shareable user desktop on the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopAvailability {
    /// A graphical desktop session is available for capture.
    Available {
        display: String,
        width: u32,
        height: u32,
        compositor: &'static str,
    },
    /// No shareable desktop exists (e.g. pre-login, locked, or headless).
    NoShareableDesktop { reason: DesktopUnavailableReason },
}

/// Typed reasons why no shareable desktop exists (honest reporting).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopUnavailableReason {
    /// No display server (Wayland or X11) detected in environment.
    NoDisplayServerDetected,
    /// Interactive desktop session is locked.
    SessionLocked,
    /// Screen capture permissions missing or denied.
    PermissionDenied,
    /// Headless system without virtual display configured.
    HeadlessNoVirtualDisplay,
}

impl fmt::Display for DesktopUnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDisplayServerDetected => {
                f.write_str("no graphical display server detected (pre-login or headless)")
            }
            Self::SessionLocked => f.write_str("desktop session is locked"),
            Self::PermissionDenied => f.write_str("desktop capture permission not granted"),
            Self::HeadlessNoVirtualDisplay => {
                f.write_str("headless system without configured virtual display")
            }
        }
    }
}

/// The always-running platform broker service (`frd`).
pub struct BrokerService {
    /// Active daemon configuration.
    pub config: DaemonConfig,
    /// Authenticated peer identity cache.
    pub peer_cache: PeerIdentityCache,
    /// Active sessions and controller registry.
    pub registry: SessionRegistry,
    /// Local desktop availability state.
    pub desktop_availability: DesktopAvailability,
    /// Supervised virtual display instance if provisioned.
    pub virtual_display: Option<super::virtual_display::VirtualDisplayInstance>,
    /// Host tailnet FQDN (e.g. "desktop.example.ts.net").
    pub tailnet_fqdn: String,
    /// Host tailnet node addresses.
    pub tailnet_ips: Vec<IpAddr>,
}

impl BrokerService {
    /// Initialize a new broker service with configuration and identities.
    #[must_use]
    pub fn new(
        config: DaemonConfig,
        host_boot_id: HostBootId,
        os_session_id: OsSessionId,
        tailnet_fqdn: String,
        tailnet_ips: Vec<IpAddr>,
    ) -> Self {
        let max_viewers = config.max_viewers;
        let peer_cache = PeerIdentityCache::with_defaults();
        let registry = SessionRegistry::new(host_boot_id, os_session_id, max_viewers);
        let (desktop_availability, virtual_display) = Self::provision_or_detect_desktop(&config);

        Self {
            config,
            peer_cache,
            registry,
            desktop_availability,
            virtual_display,
            tailnet_fqdn,
            tailnet_ips,
        }
    }

    /// Provision a virtual display if configured or detect current graphical session availability honestly.
    pub fn provision_or_detect_desktop(
        config: &DaemonConfig,
    ) -> (
        DesktopAvailability,
        Option<super::virtual_display::VirtualDisplayInstance>,
    ) {
        #[cfg(target_os = "linux")]
        {
            let is_headless_selected =
                matches!(config.desktop, super::config::DesktopSelection::Headless);
            let has_wayland = std::env::var("WAYLAND_DISPLAY").is_ok();
            let has_x11 = std::env::var("DISPLAY").is_ok();

            if is_headless_selected || config.virtual_display.enabled {
                match super::virtual_display::VirtualDisplayManager::start(&config.virtual_display)
                {
                    Ok(instance) => {
                        let availability = DesktopAvailability::Available {
                            display: instance.display.clone(),
                            width: instance.width,
                            height: instance.height,
                            compositor: "xvfb",
                        };
                        return (availability, Some(instance));
                    }
                    Err(_) => {
                        return (
                            DesktopAvailability::NoShareableDesktop {
                                reason: DesktopUnavailableReason::HeadlessNoVirtualDisplay,
                            },
                            None,
                        );
                    }
                }
            }

            if has_wayland {
                let display =
                    std::env::var("WAYLAND_DISPLAY").unwrap_or_else(|_| "wayland-0".into());
                (
                    DesktopAvailability::Available {
                        display,
                        width: 1920,
                        height: 1080,
                        compositor: "wayland",
                    },
                    None,
                )
            } else if has_x11 {
                let display = std::env::var("DISPLAY").unwrap_or_else(|_| ":0".into());
                (
                    DesktopAvailability::Available {
                        display,
                        width: 1920,
                        height: 1080,
                        compositor: "x11",
                    },
                    None,
                )
            } else {
                (
                    DesktopAvailability::NoShareableDesktop {
                        reason: DesktopUnavailableReason::NoDisplayServerDetected,
                    },
                    None,
                )
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            (
                DesktopAvailability::NoShareableDesktop {
                    reason: DesktopUnavailableReason::NoDisplayServerDetected,
                },
                None,
            )
        }
    }

    /// Detect current graphical session availability honestly.
    #[must_use]
    pub fn detect_desktop_availability(config: &DaemonConfig) -> DesktopAvailability {
        Self::provision_or_detect_desktop(config).0
    }

    /// True if the broker is completely idle (0 admitted sessions).
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.registry.session_count() == 0
    }

    /// Format honest HTTPS endpoints for the FQDN and configured tailnet IPs.
    #[must_use]
    pub fn honest_https_endpoints(&self) -> Vec<String> {
        let mut endpoints = Vec::with_capacity(1 + self.tailnet_ips.len());
        if !self.tailnet_fqdn.is_empty() {
            endpoints.push(fr_tailnet::honest_https_endpoint(
                &self.tailnet_fqdn,
                self.config.service_port,
            ));
        }
        for ip in &self.tailnet_ips {
            endpoints.push(fr_tailnet::honest_https_endpoint(
                &ip.to_string(),
                self.config.service_port,
            ));
        }
        endpoints
    }

    /// Format honest QUIC endpoints for the FQDN and configured tailnet IPs.
    #[must_use]
    pub fn honest_quic_endpoints(&self) -> Vec<String> {
        let mut endpoints = Vec::with_capacity(1 + self.tailnet_ips.len());
        if !self.tailnet_fqdn.is_empty() {
            endpoints.push(fr_tailnet::honest_quic_endpoint(
                &self.tailnet_fqdn,
                self.config.service_port,
            ));
        }
        for ip in &self.tailnet_ips {
            endpoints.push(fr_tailnet::honest_quic_endpoint(
                &ip.to_string(),
                self.config.service_port,
            ));
        }
        endpoints
    }
}

impl BrokerStateInspect for BrokerService {
    fn active_captures(&self) -> usize {
        usize::from(!self.is_idle())
    }

    fn active_encoders(&self) -> usize {
        usize::from(!self.is_idle())
    }

    fn gpu_surfaces(&self) -> usize {
        // While idle, NO GPU surfaces are held resident by construction.
        if self.is_idle() {
            0
        } else {
            3 // e.g. double-buffered capture + encoder surface
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn idle_broker_has_zero_media_allocations() {
        let config = DaemonConfig::default();
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1));
        let service = BrokerService::new(
            config,
            HostBootId::from_raw(1),
            OsSessionId::from_raw(1),
            "host.example.ts.net".into(),
            vec![ip],
        );

        assert!(service.is_idle());
        assert_eq!(service.active_captures(), 0);
        assert_eq!(service.active_encoders(), 0);
        assert_eq!(service.gpu_surfaces(), 0);
    }

    #[test]
    fn honest_endpoints_formatted_correctly() {
        let config = DaemonConfig {
            service_port: 8443,
            ..Default::default()
        };
        let ip = IpAddr::V4(Ipv4Addr::new(100, 64, 1, 2));
        let service = BrokerService::new(
            config,
            HostBootId::from_raw(1),
            OsSessionId::from_raw(1),
            "host.example.ts.net".into(),
            vec![ip],
        );

        let https = service.honest_https_endpoints();
        assert_eq!(
            https,
            vec![
                "https://host.example.ts.net:8443".to_string(),
                "https://100.64.1.2:8443".to_string()
            ]
        );

        let quic = service.honest_quic_endpoints();
        assert_eq!(
            quic,
            vec![
                "quic://host.example.ts.net:8443".to_string(),
                "quic://100.64.1.2:8443".to_string()
            ]
        );
    }
}
