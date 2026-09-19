//! The native desktop client's implemented observation profile.
//!
//! This is application capability negotiation, not a codec/transport probe or
//! authority. The caller still performs installed-tailnet/TLS admission and
//! explicitly opts into the currently unqualified native transport.
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    attachment, decoder, display,
    negotiation::{Capability, Offer, Role},
    receiver_metrics, recovery_request,
};

/// Offer the same bounded recovery and solicited decoder-load paths used by the
/// canonical running viewer. Optional means an older host can omit a feature;
/// it NEVER means the client may send its records without positive selection.
/// All four bootstrap capabilities remain mandatory. No input, clipboard,
/// audio, file, visible-presentation or control capability is implied.
///
/// Reconnection builds a fresh offer and session. Recovery stays inside the
/// existing connection/decoder under the original failure deadline; namespace
/// exhaustion and native failures retain their existing terminal behavior.
pub fn observation_offer() -> Offer {
    let mut capabilities: Vec<_> = [
        (display::CAPABILITY, 1, true),
        (decoder::CAPABILITY, 1, true),
        (attachment::CAPABILITY, 1, true),
        (attachment::DELIVERY_CAPABILITY, 1, true),
        (
            receiver_metrics::CAPABILITY,
            receiver_metrics::VERSION,
            false,
        ),
        (
            recovery_request::CAPABILITY,
            recovery_request::VERSION,
            false,
        ),
    ]
    .into_iter()
    .map(|(name, version, required)| Capability {
        name: name.into(),
        version,
        required,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::Observe,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
    }
}
