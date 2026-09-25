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
