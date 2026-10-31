//! Reports from an acknowledged, pre-observation native capability inspection.
//! Advertisement is not codec admission, display inventory or input authority.
use super::RobotInspectData;
use fr_wire::{
    inspection,
    negotiation::{self, Offer, Role},
};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvertisedCapability {
    pub name: String,
    pub version: u16,
    pub required: bool,
}

/// Exactly the seven limits carried in `HostCapabilities`. Local-only defaults
/// in `ProtocolLimits` are absent because the peer never reported them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvertisedLimits {
    pub max_control_message_bytes: u32,
    pub max_clipboard_item_bytes: u32,
    pub max_encoded_access_unit_bytes: u32,
    pub max_dimension_pixels: u32,
    pub max_coded_pixels: u64,
    pub reassembly_window_pictures: u8,
    pub per_viewer_compressed_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    AdvertisedCapabilityIntersection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    InspectionAcknowledged,
}

/// Construct only after the original probe's bootstrap `Closed` exchange. This
/// metadata snapshot grants no reusable observation or input authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct CapabilityInspectionData {
    #[serde(flatten)]
    pub peer: RobotInspectData,
    pub scope: Scope,
    pub completion: Completion,
    pub protocol_versions: Vec<u16>,
    pub profile: u8,
    pub profile_version: u16,
    pub capabilities: Vec<AdvertisedCapability>,
    pub limits: AdvertisedLimits,
    pub observation_opened: bool,
    pub input_requested: bool,
    pub native_workers_started: bool,
    pub live_capability_admission_verified: bool,
}

impl CapabilityInspectionData {
    pub fn after_acknowledged_probe(
        mut peer: RobotInspectData,
        offer: Offer,
    ) -> Result<Self, negotiation::Error> {
        offer.validate()?;
        if offer.role != Role::Observe
            || !offer.capabilities.iter().any(|capability| {
                capability.name == inspection::CAPABILITY
                    && capability.version == inspection::VERSION
                    && capability.required
            })
        {
            return Err(negotiation::Error::RequiredCapability);
        }
        peer.state = "capabilities_observed".into();
        peer.displays_count = None;
        peer.requires_approval = None;
        let limits = offer.limits;
        Ok(Self {
            peer,
            scope: Scope::AdvertisedCapabilityIntersection,
            completion: Completion::InspectionAcknowledged,
            protocol_versions: offer.versions,
            profile: offer.profile,
            profile_version: offer.profile_version,
            capabilities: offer
                .capabilities
                .into_iter()
                .map(|capability| AdvertisedCapability {
                    name: capability.name,
                    version: capability.version,
                    required: capability.required,
                })
                .collect(),
            limits: AdvertisedLimits {
                max_control_message_bytes: limits.max_control_message_bytes(),
                max_clipboard_item_bytes: limits.max_clipboard_item_bytes(),
                max_encoded_access_unit_bytes: limits.max_encoded_access_unit_bytes(),
                max_dimension_pixels: limits.max_dimension_pixels(),
                max_coded_pixels: limits.max_coded_pixels(),
                reassembly_window_pictures: limits.reassembly_window_pictures(),
                per_viewer_compressed_bytes: limits.per_viewer_compressed_bytes(),
            },
            observation_opened: false,
            input_requested: false,
            native_workers_started: false,
            live_capability_admission_verified: false,
        })
    }

    pub fn render_human(&self) -> String {
        let mut text = self.peer.render_human();
        let _ = writeln!(
            text,
            "  Capability scope: advertised intersection (protocol profile {} v{})",
            self.profile, self.profile_version
        );
        for capability in &self.capabilities {
            let _ = writeln!(
                text,
                "    {} v{}{}",
                capability.name,
                capability.version,
                if capability.required {
                    " (required)"
                } else {
                    ""
                }
            );
        }
        let _ = writeln!(
            text,
            "  Reported limits: control {} bytes; clipboard {} bytes; encoded frame {} bytes; dimensions {} pixels; coded pixels {}; reassembly {} pictures; retained compressed media {} bytes",
            self.limits.max_control_message_bytes,
            self.limits.max_clipboard_item_bytes,
            self.limits.max_encoded_access_unit_bytes,
            self.limits.max_dimension_pixels,
            self.limits.max_coded_pixels,
            self.limits.reassembly_window_pictures,
            self.limits.per_viewer_compressed_bytes
        );
        text.push_str("  Inspection acknowledged and closed before observation. Display inventory, approval policy and live codec admission remain unknown.\n");
        text
    }
}

#[cfg(test)]
mod tests;
