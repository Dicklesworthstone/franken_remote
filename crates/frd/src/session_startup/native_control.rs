//! Shared checks for the explicit native control bootstrap, not another owner.
//! Metadata comes from the selected display and completed configuration channel.
use fr_core::limits::ProtocolLimits;
use fr_core::{
    input::{DesktopPoint, InputBounds, InputView},
    input_submission::Capabilities,
};
use fr_wire::{
    WireError, attachment, clock,
    control::{self, Target},
    decoder,
    display::Display,
    negotiation::{Capability, Offer, Role, Selection},
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
        .chain(
            controlling
                .iter()
                .filter(|_| control)
                .map(|&(name, version)| (name, version, false)),
        )
        // Remote-cursor forwarding is optional in both modes: an older viewer
        // omits it and receives no cursor records.
        .chain([(fr_wire::cursor::CAPABILITY, fr_wire::cursor::VERSION, false)])
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
        .chain(
            FILE_CAPABILITIES
                .iter()
                .filter(|_| control && files)
                .map(|&(name, version)| (name, version, false)),
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

/// Positive audio-down selection by an OBSERVER: both peers offered it, so
/// the host locally enabled playback capture. A controller never gets audio
/// in this slice (the transport also refuses the attachment for it).
pub(crate) fn audio_selected(selection: &Selection) -> bool {
    selection.role == Role::Observe
        && selection
            .capabilities
            .iter()
            .any(|c| c.name == fr_wire::audio::CAPABILITY && c.version == fr_wire::audio::VERSION)
}

/// Whether this session sets up the drop lane: control and all three file
/// boundaries selected, and NOT the clipboard. A selected clipboard always
/// runs its own (consenting or declining) exchange on the same serialized
/// control-route handshake, and this slice never races the two; both peers
/// evaluate this same predicate on the same selection.
pub(crate) fn files_lane(selection: &Selection) -> Result<(), crate::native_files::Absence> {
    use crate::native_files::Absence;
    if selection.role != Role::RequestControl
        || !FILE_CAPABILITIES
            .iter()
            .all(|&(name, version)| selected(selection, name, version))
    {
        return Err(Absence::NotNegotiated);
    }
    if super::clipboard::selected(selection).is_ok() {
        return Err(Absence::WithClipboard);
    }
    Ok(())
}

pub(super) fn profile(selection: &Selection, control: bool) -> bool {
    let role = if control {
        Role::RequestControl
    } else {
        Role::Observe
    };
    selection.role == role
        && selection.validate().is_ok()
        && [
            fr_wire::display::CAPABILITY,
            attachment::CAPABILITY,
            attachment::DELIVERY_CAPABILITY,
            decoder::CAPABILITY,
        ]
        .iter()
        .all(|name| selected(selection, name, 1))
        && (!control
            || ([
                (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
                (crate::input_quic::grant::CAPABILITY, 1),
                (clock::CAPABILITY, clock::VERSION),
                (presented::CAPABILITY, presented::VERSION),
            ]
            .iter()
            .all(|(name, version)| selected(selection, name, *version))
                && selection.limits.max_control_message_bytes() as usize
                    >= control::GRANTED_BYTES.max(presented::BYTES)))
}
fn selected(selection: &Selection, name: &str, version: u16) -> bool {
    selection
        .capabilities
        .iter()
        .any(|c| c.name == name && c.version == version)
}
/// Reconstruct only the selected desktop coordinate bounds, never source age,
/// visibility, authority, native support, or the user's local approval decision.
pub(super) fn target(
    display: Display,
    binding: decoder::Binding,
    capabilities: Capabilities,
) -> Result<Target, WireError> {
    if display.handle != binding.display || display.geometry != binding.geometry {
        return Err(WireError::InvalidBinding);
    }
    binding.validate()?;
    let bounds = InputBounds::new(
        DesktopPoint {
            x: display.x,
            y: display.y,
        },
        display.pixel_width,
        display.pixel_height,
    )
    .ok_or(WireError::InvalidValue)?;
    Ok(Target {
        display_binding: binding.parent.id,
        view: InputView {
            geometry: binding.geometry,
            viewport: binding.viewport,
            configuration: binding.configuration,
            recovery: binding.recovery,
        },
        bounds,
        capabilities,
    })
}

/// The native bootstrap's selected display remains the outer control boundary.
/// Capabilities still come from independent local probes/consent; only this
/// exact display's coordinates and generations can be approved or kept active.
pub(super) fn check_target(
    display: Display,
    binding: decoder::Binding,
    candidate: Target,
) -> Result<(), WireError> {
    if target(display, binding, candidate.capabilities)? != candidate {
        return Err(WireError::InvalidBinding);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
