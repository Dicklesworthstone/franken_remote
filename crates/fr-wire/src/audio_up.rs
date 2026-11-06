#![forbid(unsafe_code)]
//! Independent controller-microphone attachment selection. Neither playback
//! audio, a control grant, nor an attachment is microphone consent. The session
//! must retain the original controller authority, separate host approval and
//! explicit local talk/OS permission before native capture or virtual output.
//!
//! This codec does not advertise an endpoint or qualify a device. Public native
//! offers must omit it until their complete capture, transport, approval,
//! indicator and endpoint paths are installed.
use crate::negotiation::{Capability, Error, MAX_CAPABILITIES, Offer, Role, Selection};

pub const CAPABILITY: &str = "native-audio-up";
pub const VERSION: u16 = 1;

/// Add the separately enabled microphone profile to a validated native offer.
/// Call only after the application's capture/endpoint and local consent UI are
/// configured. An optional diagnostic may yield its slot; no authority, media
/// or file capability is displaced when the fixed capability bound is full.
pub fn with_microphone(mut offer: Offer) -> Result<Offer, Error> {
    offer.validate()?;
    if ![
        (crate::attachment::CAPABILITY, crate::attachment::VERSION),
        (
            crate::attachment::INPUT_CAPABILITY,
            crate::attachment::INPUT_VERSION,
        ),
    ]
    .iter()
    .all(|(name, version)| {
        offer
            .capabilities
            .iter()
            .any(|cap| cap.name == *name && cap.version == *version)
    }) {
        return Err(Error::Selection);
    }
    if let Some(cap) = offer.capabilities.iter().find(|cap| cap.name == CAPABILITY) {
        return if cap.version == VERSION {
            Ok(offer)
        } else {
            Err(Error::Version)
        };
    }
    if offer.capabilities.len() == MAX_CAPABILITIES {
        offer.capabilities.retain(|cap| {
            cap.name != crate::quality_decision::CAPABILITY
                || cap.version != crate::quality_decision::VERSION
                || cap.required
        });
    }
    if offer.capabilities.len() >= MAX_CAPABILITIES {
        return Err(Error::Limits);
    }
    offer.capabilities.push(Capability {
        name: CAPABILITY.into(),
        version: VERSION,
        required: false,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer.validate()?;
    Ok(offer)
}

/// Exact positive selection of the independent microphone profile. Uplink
/// needs the original control connection's media and input attachments. It is
/// independent of audio-down: requesting playback cannot enable a microphone,
/// and a microphone-only session need not request host playback.
pub fn uplink_selected(selection: &Selection) -> bool {
    selection.role == Role::RequestControl
        && selection.validate().is_ok()
        && [
            (CAPABILITY, VERSION),
            (crate::attachment::CAPABILITY, crate::attachment::VERSION),
            (
                crate::attachment::INPUT_CAPABILITY,
                crate::attachment::INPUT_VERSION,
            ),
        ]
        .iter()
        .all(|&(name, version)| {
            selection
                .capabilities
                .iter()
                .any(|cap| cap.name == name && cap.version == version)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{attachment, negotiation::Capability};
    use fr_core::limits::ProtocolLimits;

    fn selection() -> Selection {
        let mut capabilities: Vec<_> = [
            (CAPABILITY, VERSION),
            (attachment::CAPABILITY, attachment::VERSION),
            (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        ]
        .iter()
        .map(|&(name, version)| Capability {
            name: name.into(),
            version,
            required: false,
        })
        .collect();
        capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        Selection {
            version: 0,
            profile: 1,
            profile_version: 0,
            role: Role::RequestControl,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        }
    }

    fn offer(role: Role) -> Offer {
        Offer {
            versions: vec![0],
            profile: 1,
            profile_version: 0,
            role,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities: selection()
                .capabilities
                .into_iter()
                .filter(|cap| cap.name != CAPABILITY)
                .collect(),
        }
    }
    fn fill(offer: &mut Offer) {
        while offer.capabilities.len() < MAX_CAPABILITIES {
            offer.capabilities.push(Capability {
                name: format!("test-feature-{:02}", offer.capabilities.len()),
                version: 1,
                required: false,
            });
        }
        offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    }
    #[test]
    fn public_microphone_offer_is_optional_bilateral_and_idempotent() {
        let client = with_microphone(offer(Role::RequestControl)).unwrap();
        let legacy = offer(Role::Observe);
        let selected = client.intersect(&legacy).unwrap();
        assert!(
            selected
                .capabilities
                .iter()
                .all(|cap| cap.name != CAPABILITY)
        );
        let host = with_microphone(legacy).unwrap();
        let selected = client.intersect(&host).unwrap();
        let mic = selected
            .capabilities
            .iter()
            .find(|cap| cap.name == CAPABILITY)
            .unwrap();
        assert_eq!(mic.version, VERSION);
        assert!(!mic.required);
        assert_eq!(with_microphone(client.clone()), Ok(client));
    }
    #[test]
    fn microphone_can_replace_only_an_optional_diagnostic_at_capacity() {
        let mut base = offer(Role::Observe);
        base.capabilities.push(Capability {
            name: crate::quality_decision::CAPABILITY.into(),
            version: crate::quality_decision::VERSION,
            required: false,
        });
        fill(&mut base);
        let selected = with_microphone(base.clone()).unwrap();
        assert_eq!(selected.capabilities.len(), MAX_CAPABILITIES);
        for capability in base
            .capabilities
            .iter()
            .filter(|cap| cap.name != crate::quality_decision::CAPABILITY)
        {
            assert!(selected.capabilities.contains(capability));
        }
        base.capabilities
            .iter_mut()
            .find(|cap| cap.name == crate::quality_decision::CAPABILITY)
            .unwrap()
            .required = true;
        assert_eq!(with_microphone(base), Err(Error::Limits));
        let mut full = offer(Role::RequestControl);
        fill(&mut full);
        assert_eq!(with_microphone(full), Err(Error::Limits));
    }
    #[test]
    fn public_microphone_refuses_an_observer_without_input_or_an_unknown_version() {
        let mut observation = offer(Role::Observe);
        observation
            .capabilities
            .retain(|cap| cap.name != attachment::INPUT_CAPABILITY);
        assert_eq!(with_microphone(observation), Err(Error::Selection));
        let mut unknown = with_microphone(offer(Role::RequestControl)).unwrap();
        unknown
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == CAPABILITY)
            .unwrap()
            .version += 1;
        assert_eq!(with_microphone(unknown), Err(Error::Version));
    }

    #[test]
    fn microphone_is_controller_only_and_independent_of_playback() {
        let mut selected = selection();
        assert!(uplink_selected(&selected));
        assert!(!crate::audio_control::downlink_selected(&selected));
        selected.role = Role::Observe;
        assert!(!uplink_selected(&selected));
        selected.capabilities.push(Capability {
            name: crate::audio::CAPABILITY.into(),
            version: crate::audio::VERSION,
            required: false,
        });
        selected.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        assert!(crate::audio_control::downlink_selected(&selected));
        assert!(!uplink_selected(&selected));
    }

    #[test]
    fn every_prerequisite_requires_exact_bilateral_positive_selection() {
        let selected = selection();
        for index in 0..selected.capabilities.len() {
            let mut missing = selected.clone();
            missing.capabilities.remove(index);
            assert!(!uplink_selected(&missing));
            let mut future = selected.clone();
            future.capabilities[index].version += 1;
            assert!(!uplink_selected(&future));
        }
        let mut duplicate = selected;
        duplicate
            .capabilities
            .insert(0, duplicate.capabilities[0].clone());
        assert!(!uplink_selected(&duplicate));
    }

    #[test]
    fn playback_and_control_audio_do_not_infer_microphone_support() {
        let mut selected = selection();
        selected.capabilities.retain(|cap| cap.name != CAPABILITY);
        for (name, version) in [
            (crate::audio::CAPABILITY, crate::audio::VERSION),
            (
                crate::audio_control::CAPABILITY,
                crate::audio_control::VERSION,
            ),
        ] {
            selected.capabilities.push(Capability {
                name: name.into(),
                version,
                required: false,
            });
        }
        selected.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        assert!(crate::audio_control::downlink_selected(&selected));
        assert!(!uplink_selected(&selected));
    }
}
