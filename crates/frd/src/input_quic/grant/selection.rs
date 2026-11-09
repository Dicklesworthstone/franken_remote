//! The broker's negotiated request boundary, checked before approval intake.
//! This owns no authority, OS state, queue, timer or allocation.
#![forbid(unsafe_code)]

use fr_core::{input_submission::Capability, limits::ProtocolLimits};
use fr_wire::{
    WireError,
    control::{self, Request},
    input::{InputDelivery, InputDirection},
    negotiation::{ControlBinding, Selection},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Error {
    Wire(WireError),
    NotNegotiated,
}

/// Frozen from the original authenticated connection's validated selection.
/// The local executor's broader probe cannot upgrade a peer's selected modes.
pub(super) struct Selected {
    pixels: bool,
}

impl Selected {
    pub(super) fn new(selection: &Selection) -> Self {
        Self {
            pixels: control::requestable(&selection.capabilities).contains(Capability::PixelScroll),
        }
    }

    pub(super) fn decode(
        &self,
        bytes: &[u8],
        parent: ControlBinding,
        limits: &ProtocolLimits,
    ) -> Result<Request, Error> {
        let request = control::decode_request(
            bytes,
            parent,
            limits,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .map_err(Error::Wire)?;
        if request
            .target
            .capabilities
            .contains(Capability::PixelScroll)
            && !self.pixels
        {
            return Err(Error::NotNegotiated);
        }
        // Preserve the original request exactly. Approval and the probed
        // executor still decide its operations; this never clamps a request.
        Ok(request)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fr_core::{
        ids::{
            CodecConfigurationGeneration, DisplayGeometryGeneration, HostBootId, OsSessionId,
            RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
        },
        input::{DesktopPoint, InputBounds, InputView},
        input_submission::Capabilities,
    };
    use fr_wire::negotiation::{Capability as Negotiated, Offer, Role};

    fn request(scroll: Capability) -> Request {
        Request {
            parent: ControlBinding {
                id: 7,
                host_boot: HostBootId::from_raw(1),
                os_session: OsSessionId::from_raw(2),
                remote_session: RemoteSessionId::from_raw(3),
            },
            sequence: 11,
            target: control::Target {
                display_binding: 8,
                view: InputView {
                    geometry: DisplayGeometryGeneration::INITIAL,
                    viewport: ViewportMappingGeneration::INITIAL,
                    configuration: CodecConfigurationGeneration::INITIAL,
                    recovery: RecoveryGeneration::INITIAL,
                },
                bounds: InputBounds::new(DesktopPoint { x: 0, y: 0 }, 1920, 1080).unwrap(),
                capabilities: Capabilities::default()
                    .with(Capability::Absolute)
                    .with(scroll),
            },
        }
    }

    fn bytes(request: Request) -> Vec<u8> {
        let mut bytes = vec![0; control::REQUEST_BYTES];
        let length = control::encode_request(
            request,
            &mut bytes,
            &ProtocolLimits::ABSOLUTE,
            InputDirection::ViewerToHost,
            InputDelivery::Reliable,
        )
        .unwrap();
        bytes.truncate(length);
        bytes
    }

    fn selected(marker: Option<(&str, u16)>) -> Selected {
        let mut capabilities = vec![Negotiated {
            name: control::GRANT_CAPABILITY.into(),
            version: 1,
            required: true,
        }];
        if let Some((name, version)) = marker {
            capabilities.push(Negotiated {
                name: name.into(),
                version,
                required: false,
            });
        }
        capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let offer = Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        };
        Selected::new(&offer.select().unwrap())
    }

    #[test]
    fn unselected_precise_request_is_refused_before_approval_intake() {
        let request = request(Capability::PixelScroll);
        let bytes = bytes(request);
        for marker in [
            None,
            Some((control::LINE_SCROLL_CAPABILITY, 1)),
            Some((control::PIXEL_SCROLL_CAPABILITY, 2)),
            Some(("native-input-pixel-scrolls", 1)),
        ] {
            assert_eq!(
                selected(marker).decode(&bytes, request.parent, &ProtocolLimits::ABSOLUTE),
                Err(Error::NotNegotiated)
            );
        }
        let selected = selected(Some((control::PIXEL_SCROLL_CAPABILITY, 1)));
        assert_eq!(
            selected.decode(&bytes, request.parent, &ProtocolLimits::ABSOLUTE),
            Ok(request)
        );
    }

    #[test]
    fn legacy_requests_keep_their_exact_capabilities_and_identity() {
        for capability in [Capability::Keys, Capability::LineScroll] {
            let request = request(capability);
            assert_eq!(
                selected(None).decode(&bytes(request), request.parent, &ProtocolLimits::ABSOLUTE),
                Ok(request)
            );
        }
    }

    #[test]
    fn selected_scroll_never_bypasses_wire_or_parent_validation() {
        let request = request(Capability::PixelScroll);
        let bytes = bytes(request);
        let selected = selected(Some((control::PIXEL_SCROLL_CAPABILITY, 1)));
        assert!(matches!(
            selected.decode(
                &bytes[..bytes.len() - 1],
                request.parent,
                &ProtocolLimits::ABSOLUTE
            ),
            Err(Error::Wire(_))
        ));
        let other = ControlBinding {
            remote_session: RemoteSessionId::from_raw(9),
            ..request.parent
        };
        assert!(matches!(
            selected.decode(&bytes, other, &ProtocolLimits::ABSOLUTE),
            Err(Error::Wire(_))
        ));
    }
}
