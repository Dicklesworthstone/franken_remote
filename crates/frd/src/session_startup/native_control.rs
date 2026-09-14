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

#[cfg(test)]
mod tests;
