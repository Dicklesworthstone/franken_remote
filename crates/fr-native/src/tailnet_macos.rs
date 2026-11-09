//! Compatibility entry point for installed-Tailscale discovery.
//!
//! `fr-tailnet` reads the installed app's loopback `LocalAPI`, or without the
//! app the open-source daemon's root socket, for discovery, destination
//! resolution, TLS dialing and identity renewal alike.
use asupersync::cx::Cx;
use fr_tailnet::{Discovery, Error, LocalApi};

/// Discover machines through the installed Tailscale (`LocalApi::installed`).
/// A discovery snapshot is informational and never grants desktop authority.
pub async fn discover(cx: &Cx) -> Result<Discovery, Error> {
    LocalApi::installed().discover(cx).await
}
