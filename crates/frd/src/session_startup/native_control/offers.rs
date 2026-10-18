//! Stateless native host offers; capability selection never grants authority.
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    attachment, clock, control, decoder,
    negotiation::{Capability, Offer, Role},
    presented,
};

/// The native host's offer. The four observation bootstrap capabilities are
/// always required. With `control`, the four explicit control boundaries are
/// offered OPTIONAL: an observer's intersection drops them, while a
/// controller's own required copies select them. Offering is never a grant,
/// and without `control` a controller's offer fails as `RequiredCapability`.
/// The role of the host's own offer is not negotiated (the intersection
/// carries the peer's role).
pub fn host_offer(control: bool) -> Offer {
    host_offer_with(control, false)
}
/// The three boundaries of the optional text clipboard (its attachment role,
/// record codec and bilateral readiness). Offered OPTIONAL by both sides, so
/// either side's absence drops them and control still negotiates.
pub const CLIPBOARD_CAPABILITIES: [(&str, u16); 3] = [
    (
        attachment::CLIPBOARD_CAPABILITY,
        attachment::CLIPBOARD_VERSION,
    ),
    (fr_wire::clipboard::CAPABILITY, fr_wire::clipboard::VERSION),
    (
        fr_wire::clipboard::startup::CAPABILITY,
        fr_wire::clipboard::startup::VERSION,
    ),
];
/// `host_offer`, plus (only with `control` AND the operator's local clipboard
/// enable) the optional clipboard boundaries. Selecting them is never consent:
/// the host still needs its own configured owner, and the lane attaches only
/// under the controller's active input attachment.
pub fn host_offer_with(control: bool, clipboard: bool) -> Offer {
    host_offer_with_audio(control, clipboard, false)
}
/// [`host_offer_with`] plus, only when the operator LOCALLY enabled playback
/// capture, the optional audio-down capability. Offering it is not an
/// approval: audio still waits for this session's admitted observation, and a
/// viewer that does not offer it receives no audio record.
pub fn host_offer_with_audio(control: bool, clipboard: bool, audio: bool) -> Offer {
    host_offer_with_files(control, clipboard, audio, false)
}
/// The three boundaries of the optional viewer-to-host drop lane: its
/// attachment role, the ATP full-object envelope and channel-derived scope
/// (no out-of-band handle, no host path). Offered OPTIONAL by both sides.
pub const FILE_CAPABILITIES: [(&str, u16); 3] = [
    (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
    (fr_wire::files::CAPABILITY, fr_wire::files::VERSION),
    (
        fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
        fr_wire::files::CHANNEL_SCOPE_VERSION,
    ),
];
/// [`host_offer_with_audio`] plus, only with `control` AND the operator's
/// local `--files` drop directory, the optional file boundaries. Selecting
/// them is not a transfer: the lane is offered only on the controller's own
/// session after its grant, and attaches only under its active input lease.
// Independent operator opt-ins, each one plain capability switch.
#[allow(clippy::fn_params_excessive_bools)]
pub fn host_offer_with_files(control: bool, clipboard: bool, audio: bool, files: bool) -> Offer {
    let observation = [
        (fr_wire::display::CAPABILITY, 1),
        (decoder::CAPABILITY, 1),
        (attachment::CAPABILITY, 1),
        (attachment::DELIVERY_CAPABILITY, 1),
    ];
    let controlling = [
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (control::GRANT_CAPABILITY, 1),
        (clock::CAPABILITY, clock::VERSION),
        (presented::CAPABILITY, presented::VERSION),
    ];
    let mut capabilities: Vec<_> = observation
        .iter()
        .map(|&(name, version)| (name, version, true))
        // Each endpoint advertises its existing role-specific ceiling in the
        // attachment exchange. This can only narrow the host's reserved grant.
        .chain([(
            attachment::credit::CAPABILITY,
            attachment::credit::VERSION,
            false,
        )])
        .chain(
            controlling
                .iter()
                .filter(|_| control)
                .map(|&(name, version)| (name, version, false)),
        )
        // Remote-cursor forwarding is optional in both modes: an older viewer
        // omits it and receives no cursor records.
        .chain([(fr_wire::cursor::CAPABILITY, fr_wire::cursor::VERSION, false)])
        // Reference recovery: a viewer whose reference chain broke reports it
        // and gets a fresh recovery IDR on new channels instead of a full
        // reconnect. A controller's input stays suspended meanwhile and
        // resumes on the recovered generation (see running/publisher.rs).
        .chain([(
            fr_wire::recovery_request::CAPABILITY,
            fr_wire::recovery_request::VERSION,
            false,
        )])
        .chain(
            CLIPBOARD_CAPABILITIES
                .iter()
                .filter(|_| control && clipboard)
                .map(|&(name, version)| (name, version, false)),
        )
        .chain(
            [(fr_wire::audio::CAPABILITY, fr_wire::audio::VERSION, false)]
                .into_iter()
                .filter(|_| audio),
        )
        // Legacy hosts offer audio-down even when their control path cannot
        // use it. The extra OPTIONAL boundary prevents an attachment deadlock
        // with those hosts without making audio mandatory for control.
        .chain(
            [(
                fr_wire::audio_control::CAPABILITY,
                fr_wire::audio_control::VERSION,
                false,
            )]
            .into_iter()
            .filter(|_| control && audio),
        )
        .chain(
            FILE_CAPABILITIES
                .iter()
                .filter(|_| control && files)
                .map(|&(name, version)| (name, version, false)),
        )
        // A record-size opt-in, not another clipboard/file permission. Legacy
        // peers omit it and keep their critical-sized bulk record allowance.
        .chain(
            [(
                attachment::BULK_RECORD_CAPABILITY,
                attachment::BULK_RECORD_VERSION,
                false,
            )]
            .into_iter()
            .filter(|_| control && (clipboard || files)),
        )
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

/// Only for a control-capable host whose probed input executor can scroll by
/// lines: the optional `native-input-line-scroll` boundary. A controller asks
/// for `LineScroll` only when both peers select it; without it the wheel alone
/// is unavailable and every other granted operation still works.
#[must_use]
pub fn with_line_scroll(mut offer: Offer) -> Offer {
    offer.capabilities.push(Capability {
        name: control::LINE_SCROLL_CAPABILITY.into(),
        version: control::LINE_SCROLL_VERSION,
        required: false,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer
}

#[cfg(test)]
#[path = "offers/tests.rs"]
mod tests;
