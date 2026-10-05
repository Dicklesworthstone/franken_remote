#![forbid(unsafe_code)]
//! Production offer/intersection coverage, not codec or live-device evidence.
use fr_client::native;
use fr_wire::{
    attachment, audio, audio_control, clock, control,
    negotiation::{self, Offer, Role, Selection},
    presented,
};
use frd::session_startup::{CLIPBOARD_CAPABILITIES, FILE_CAPABILITIES, host_offer_with_files};

fn selected(selection: &Selection, name: &str, version: u16) -> bool {
    selection
        .capabilities
        .iter()
        .any(|c| c.name == name && c.version == version)
}
fn negotiate(client: &Offer, host: &Offer) -> Selection {
    client.validate().unwrap();
    host.validate().unwrap();
    let intersection = host.intersect(client).unwrap();
    client.check_host(&intersection).unwrap();
    let selection = intersection.select().unwrap();
    selection.check_against(&intersection).unwrap();
    selection
}

#[test]
fn all_auxiliary_combinations_keep_control_and_independent_opt_ins() {
    for client_bits in 0_u8..8 {
        let (clipboard, files, sound) = (
            client_bits & 1 != 0,
            client_bits & 2 != 0,
            client_bits & 4 != 0,
        );
        let client = native::control_offer_with_auxiliary(clipboard, files, sound);
        assert!(client.capabilities.len() <= negotiation::MAX_CAPABILITIES);
        for host_bits in 0_u8..8 {
            let host = frd::session_startup::with_line_scroll(host_offer_with_files(
                true,
                host_bits & 1 != 0,
                host_bits & 4 != 0,
                host_bits & 2 != 0,
            ));
            let selection = negotiate(&client, &host);
            assert_eq!(selection.role, Role::RequestControl);
            assert_eq!(
                audio_control::downlink_selected(&selection),
                sound && host_bits & 4 != 0
            );
            for (family, enabled) in [
                (CLIPBOARD_CAPABILITIES, clipboard && host_bits & 1 != 0),
                (FILE_CAPABILITIES, files && host_bits & 2 != 0),
            ] {
                for (name, version) in family {
                    assert_eq!(selected(&selection, name, version), enabled);
                }
            }
            for name in [
                attachment::INPUT_CAPABILITY,
                control::GRANT_CAPABILITY,
                clock::CAPABILITY,
                presented::CAPABILITY,
            ] {
                assert!(
                    selection
                        .capabilities
                        .iter()
                        .any(|c| c.name == name && c.required)
                );
            }
            assert!(selected(
                &selection,
                fr_wire::cursor::CAPABILITY,
                fr_wire::cursor::VERSION,
            ));
            assert!(selected(
                &selection,
                fr_wire::recovery_request::CAPABILITY,
                fr_wire::recovery_request::VERSION,
            ));
        }
    }
}

#[test]
fn no_audio_keeps_each_existing_control_offer_unchanged() {
    for (clipboard, files, original) in [
        (false, false, native::control_offer()),
        (true, false, native::control_offer_with_clipboard()),
        (false, true, native::control_offer_with_files()),
        (true, true, native::control_offer_with_clipboard_and_files()),
    ] {
        assert_eq!(
            native::control_offer_with_auxiliary(clipboard, files, false),
            original
        );
    }
}

#[test]
fn legacy_observer_only_audio_never_requires_a_control_audio_attachment() {
    let client = native::control_offer_with_auxiliary(true, true, true);
    let mut legacy = host_offer_with_files(true, true, true, true);
    legacy
        .capabilities
        .retain(|c| c.name != audio_control::CAPABILITY);
    let selection = negotiate(&client, &legacy);
    assert_eq!(selection.role, Role::RequestControl);
    assert!(selected(&selection, audio::CAPABILITY, audio::VERSION));
    assert!(!audio_control::downlink_selected(&selection));
}

#[test]
fn neither_audio_boundary_is_mandatory_and_both_are_needed() {
    let client = native::control_offer_with_auxiliary(false, false, true);
    for missing in [audio::CAPABILITY, audio_control::CAPABILITY] {
        let mut host = host_offer_with_files(true, false, true, false);
        host.capabilities.retain(|c| c.name != missing);
        let selection = negotiate(&client, &host);
        assert_eq!(selection.role, Role::RequestControl);
        assert!(!audio_control::downlink_selected(&selection));
    }
    for cap in client
        .capabilities
        .iter()
        .filter(|c| c.name == audio::CAPABILITY || c.name == audio_control::CAPABILITY)
    {
        assert!(!cap.required);
    }
}

#[test]
fn audio_cannot_make_an_observation_only_host_control_capable() {
    let client = native::control_offer_with_auxiliary(true, true, true);
    let host = host_offer_with_files(false, false, true, false);
    assert_eq!(
        host.intersect(&client),
        Err(negotiation::Error::RequiredCapability)
    );
}
