//! Shared checks for the explicit native control bootstrap, not another owner.
//! Metadata comes from the selected display and completed configuration channel.
use fr_core::{
    input::{DesktopPoint, InputBounds, InputView},
    input_submission::Capabilities,
};
use fr_wire::{
    WireError, attachment, clock,
    control::{self, Target},
    decoder,
    display::Display,
    negotiation::{Role, Selection},
    presented,
};

mod offers;
pub use offers::{
    CLIPBOARD_CAPABILITIES, FILE_CAPABILITIES, host_offer, host_offer_with, host_offer_with_audio,
    host_offer_with_download, host_offer_with_duplex, host_offer_with_files, with_line_scroll,
};

/// Positive playback selection, including the explicit control-audio extension.
/// A legacy audio-down capability alone still applies only to observers.
pub(crate) fn audio_selected(selection: &Selection) -> bool {
    fr_wire::audio_control::downlink_selected(selection)
}

/// Whether this session sets up the drop lane: control and all three file
/// boundaries selected. Clipboard remains independently negotiated/consented:
/// its attachment and readiness messages are dispatched to their own owner,
/// never interpreted as file permission or a file-channel ticket.
pub(crate) fn files_lane(selection: &Selection) -> Result<(), crate::native_files::Absence> {
    use crate::native_files::Absence;
    if selection.role != Role::RequestControl
        || (selected(
            selection,
            fr_wire::files::DOWNLOAD_CAPABILITY,
            fr_wire::files::DOWNLOAD_VERSION,
        ) && !fr_wire::files::duplex_selected(selection))
        || !FILE_CAPABILITIES
            .iter()
            .all(|&(name, version)| selected(selection, name, version))
    {
        return Err(Absence::NotNegotiated);
    }
    Ok(())
}

/// Positive host-to-controller selection. Each one-use attachment has one
/// direction, established by capability agreement before application payload.
/// A second upload attachment requires the complete duplex profile.
pub(crate) fn file_download_lane(
    selection: &Selection,
) -> Result<(), crate::native_files::Absence> {
    use crate::native_files::Absence;
    if selection.role != Role::RequestControl
        || (selected(
            selection,
            fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
            fr_wire::files::CHANNEL_SCOPE_VERSION,
        ) && !fr_wire::files::duplex_selected(selection))
        || ![
            (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
            (fr_wire::files::CAPABILITY, fr_wire::files::VERSION),
            (
                fr_wire::files::DOWNLOAD_CAPABILITY,
                fr_wire::files::DOWNLOAD_VERSION,
            ),
        ]
        .iter()
        .all(|&(name, version)| selected(selection, name, version))
    {
        return Err(Absence::NotNegotiated);
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
