//! Compatibility entry point for installed-app discovery.
//!
//! The authenticated child boundary lives in `fr-tailnet-platform`, allowing
//! `fr-tailnet` to use the same source for discovery, destination resolution,
//! TLS dialing and identity renewal without depending on desktop/media code.
use asupersync::cx::Cx;
use fr_tailnet::{Discovery, Error, LocalApi};

/// Discover machines through the signed installed Tailscale application.
/// A discovery snapshot is informational and never grants desktop authority.
pub async fn discover(cx: &Cx) -> Result<Discovery, Error> {
    LocalApi::installed().discover(cx).await
}
