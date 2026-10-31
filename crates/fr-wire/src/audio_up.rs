#![forbid(unsafe_code)]
//! Independent controller-microphone attachment selection. Neither playback
//! audio, a control grant, nor an attachment is microphone consent. The session
//! must retain the original controller authority, separate host approval and
//! explicit local talk/OS permission before native capture or virtual output.
//!
//! This codec does not advertise an endpoint or qualify a device. Public native
//! offers must omit it until their complete capture, transport, approval,
//! indicator and endpoint paths are installed.
use crate::negotiation::{Role, Selection};

pub const CAPABILITY: &str = "native-audio-up";
pub const VERSION: u16 = 1;

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
