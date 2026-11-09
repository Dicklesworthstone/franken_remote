//! Compatibility entry point for installed-app discovery.
//!
//! `fr-tailnet` reads the installed app's loopback `LocalAPI` for discovery,
//! destination resolution, TLS dialing and identity renewal alike.
use asupersync::cx::Cx;
use fr_tailnet::{Discovery, Error, LocalApi};

/// Discover machines through the installed Tailscale application.
/// A discovery snapshot is informational and never grants desktop authority.
pub async fn discover(cx: &Cx) -> Result<Discovery, Error> {
    LocalApi::installed().discover(cx).await
}
