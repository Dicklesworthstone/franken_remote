#![forbid(unsafe_code)]
//! Explicit extension of native playback audio to control-capable sessions.
//!
//! `native-audio-down` alone retains its legacy observer-only meaning. Both
//! peers must ALSO select `native-audio-control` v1 for AudioDown on a
//! RequestControl session. This prevents a new client waiting for a fifth
//! attachment from an older host which advertises playback but cannot serve
//! it with control. See PROTOCOL_NATIVE_AUDIO_CONTROL.md.
//!
//! This is capability selection, not observation approval, a local audio enable,
//! an input grant, microphone consent or codec/device qualification.
use crate::negotiation::{Role, Selection};

pub const CAPABILITY: &str = "native-audio-control";
pub const VERSION: u16 = 1;

/// Whether this validated selection expects one audio-down attachment.
/// Exact versions are required. A missing extension disables only optional
/// playback on a controller; it never downgrades or refuses the input grant.
pub fn downlink_selected(selection: &Selection) -> bool {
    let selected = |name, version| {
        selection
            .capabilities
            .iter()
            .any(|cap| cap.name == name && cap.version == version)
    };
    selection.validate().is_ok()
        && selected(crate::audio::CAPABILITY, crate::audio::VERSION)
        && match selection.role {
            Role::Observe => true,
            Role::RequestControl => selected(CAPABILITY, VERSION),
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::negotiation::Capability;
    use fr_core::limits::ProtocolLimits;

    fn selection(role: Role, capabilities: &[(&str, u16)]) -> Selection {
        let mut capabilities: Vec<_> = capabilities
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
            role,
            limits: ProtocolLimits::ABSOLUTE,
            capabilities,
        }
    }
    #[test]
    fn legacy_observer_playback_is_unchanged() {
        assert!(downlink_selected(&selection(
            Role::Observe,
            &[(crate::audio::CAPABILITY, crate::audio::VERSION)],
        )));
    }
    #[test]
    fn legacy_host_audio_offer_does_not_make_a_controller_wait_for_audio() {
        let selected = selection(
            Role::RequestControl,
            &[(crate::audio::CAPABILITY, crate::audio::VERSION)],
        );
        assert!(selected.validate().is_ok());
        assert!(!downlink_selected(&selected));
        assert_eq!(selected.role, Role::RequestControl);
    }
    #[test]
    fn control_playback_requires_both_exact_capabilities() {
        assert!(downlink_selected(&selection(
            Role::RequestControl,
            &[
                (crate::audio::CAPABILITY, crate::audio::VERSION),
                (CAPABILITY, VERSION),
            ],
        )));
        for capabilities in [
            vec![(CAPABILITY, VERSION)],
            vec![(crate::audio::CAPABILITY, crate::audio::VERSION), (CAPABILITY, VERSION + 1)],
            vec![(crate::audio::CAPABILITY, crate::audio::VERSION + 1), (CAPABILITY, VERSION)],
        ] {
            assert!(!downlink_selected(&selection(Role::RequestControl, &capabilities)));
        }
    }
    #[test]
    fn extension_alone_never_enables_observer_audio_either() {
        assert!(!downlink_selected(&selection(Role::Observe, &[(CAPABILITY, VERSION)])));
    }
    #[test]
    fn duplicate_capabilities_are_not_permission() {
        let mut selected = selection(
            Role::RequestControl,
            &[(crate::audio::CAPABILITY, crate::audio::VERSION), (CAPABILITY, VERSION)],
        );
        selected.capabilities.insert(0, selected.capabilities[0].clone());
        assert!(selected.validate().is_err());
        assert!(!downlink_selected(&selected));
    }
}
