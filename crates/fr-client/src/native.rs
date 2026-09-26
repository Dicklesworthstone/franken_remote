//! The native desktop client's implemented observation and control profiles.
//!
//! This is application capability negotiation, not a codec/transport probe or
//! authority. The caller still performs installed-tailnet/TLS admission and
//! explicitly opts into the currently unqualified native transport.
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    attachment, clock, control, cursor, decoder, display,
    negotiation::{Capability, Offer, Role},
    presented, receiver_metrics, recovery_request,
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
    offer(Role::Observe, &[])
}

/// The observation profile plus the OPTIONAL host playback downlink. Only an
/// explicit local request (`fr connect --audio`) uses this offer; the host
/// still decides (its own local enable) and a host without audio simply omits
/// it. It never offers or implies a microphone (uplink).
pub fn observation_offer_with_audio() -> Offer {
    offer(
        Role::Observe,
        &[(fr_wire::audio::CAPABILITY, fr_wire::audio::VERSION, false)],
    )
}

/// The observation profile plus the four boundaries the host's explicit
/// control bootstrap requires: the input attachment, the one-use control
/// grant, clock correlation and presented-state proof, all mandatory. Asking
/// for control is an intent, never permission: the host may still refuse
/// (typed), and no input is sent before its local grant and the client's own
/// presented view.
pub fn control_offer() -> Offer {
    offer(
        Role::RequestControl,
        &[
            (
                attachment::INPUT_CAPABILITY,
                attachment::INPUT_VERSION,
                true,
            ),
            (control::GRANT_CAPABILITY, 1, true),
            (clock::CAPABILITY, clock::VERSION, true),
            (presented::CAPABILITY, presented::VERSION, true),
        ],
    )
}

/// `control_offer` plus the three OPTIONAL text-clipboard boundaries (the
/// clipboard attachment role, its record codec and bilateral readiness). A
/// host without clipboard drops them and control still negotiates; the
/// client then reports that absence. Selecting them opens nothing: the lane
/// attaches only under the granted input attachment, after local consent.
pub fn control_offer_with_clipboard() -> Offer {
    offer(
        Role::RequestControl,
        &[
            (
                attachment::INPUT_CAPABILITY,
                attachment::INPUT_VERSION,
                true,
            ),
            (control::GRANT_CAPABILITY, 1, true),
            (clock::CAPABILITY, clock::VERSION, true),
            (presented::CAPABILITY, presented::VERSION, true),
            (
                attachment::CLIPBOARD_CAPABILITY,
                attachment::CLIPBOARD_VERSION,
                false,
            ),
            (
                fr_wire::clipboard::CAPABILITY,
                fr_wire::clipboard::VERSION,
                false,
            ),
            (
                fr_wire::clipboard::startup::CAPABILITY,
                fr_wire::clipboard::startup::VERSION,
                false,
            ),
        ],
    )
}

/// `control_offer` plus the three OPTIONAL drop-lane boundaries (the file
/// attachment role, the ATP full-object envelope and channel-derived scope).
/// A host without a drop directory drops them and control still negotiates;
/// the client then reports that absence. Selecting them sends nothing: the
/// lane is the host's one-use offer under the granted input attachment, and
/// only files the local user explicitly selected are ever read.
pub fn control_offer_with_files() -> Offer {
    offer(
        Role::RequestControl,
        &[
            (
                attachment::INPUT_CAPABILITY,
                attachment::INPUT_VERSION,
                true,
            ),
            (control::GRANT_CAPABILITY, 1, true),
            (clock::CAPABILITY, clock::VERSION, true),
            (presented::CAPABILITY, presented::VERSION, true),
            (
                attachment::FILES_CAPABILITY,
                attachment::FILES_VERSION,
                false,
            ),
            (fr_wire::files::CAPABILITY, fr_wire::files::VERSION, false),
            (
                fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
                fr_wire::files::CHANNEL_SCOPE_VERSION,
                false,
            ),
        ],
    )
}

fn offer(role: Role, extra: &[(&str, u16, bool)]) -> Offer {
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
        // The single client-rendered remote cursor; absent hosts send none.
        (cursor::CAPABILITY, cursor::VERSION, false),
    ]
    .iter()
    .chain(extra)
    .map(|(name, version, required)| Capability {
        name: (*name).into(),
        version: *version,
        required: *required,
    })
    .collect();
    capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities,
    }
}
