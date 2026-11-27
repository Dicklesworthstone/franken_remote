//! Real native profiles retain microphone and independently selected auxiliaries.
use super::{Startup, bytes, opened};
use fr_client::native;
use fr_wire::{
    attachment, audio_up, control,
    negotiation::{self, Message, Role},
    quality_decision,
};

#[test]
fn microphone_and_text_fit_native_auxiliary_profiles_without_dropping_features() {
    for clipboard in [false, true] {
        for audio in [false, true] {
            let profiles = [
                native::control_offer_with_auxiliary(clipboard, false, audio),
                native::control_offer_with_auxiliary(clipboard, true, audio),
                native::control_offer_with_download(clipboard, audio),
                native::control_offer_with_duplex(clipboard, audio),
            ];
            for (direction, original) in profiles.into_iter().enumerate() {
                let full_duplex_audio = clipboard && audio && direction == 3;
                let microphone = audio_up::with_microphone(original.clone()).unwrap();
                // This is the shipping Linux CLI's actual extension order.
                let configured = control::with_available_committed_text(microphone).unwrap();
                configured.validate().unwrap();
                assert_eq!(configured.role, Role::RequestControl);
                assert_eq!(configured.limits, original.limits);
                assert!(configured.capabilities.len() <= negotiation::MAX_CAPABILITIES);
                for capability in &original.capabilities {
                    let may_yield = !capability.required
                        && [
                            quality_decision::CAPABILITY,
                            attachment::BULK_RECORD_CAPABILITY,
                            attachment::credit::CAPABILITY,
                            fr_wire::receiver_metrics::CAPABILITY,
                        ]
                        .contains(&capability.name.as_str());
                    assert!(may_yield || configured.capabilities.contains(capability));
                }
                let selected = configured.select().unwrap();
                assert!(audio_up::uplink_selected(&selected));
                assert_eq!(fr_wire::audio_control::downlink_selected(&selected), audio);
                assert_eq!(fr_wire::files::duplex_selected(&selected), direction == 3);
                assert_eq!(
                    configured.capabilities.iter().any(|cap| {
                        cap.name == control::TEXT_CAPABILITY
                            && cap.version == control::TEXT_VERSION
                            && !cap.required
                    }),
                    !full_duplex_audio
                );
                let encoded = bytes(&Message::ClientHello(configured.clone()));
                assert_eq!(
                    negotiation::decode(&encoded, negotiation::MAX_RECORD, 0).unwrap(),
                    Message::ClientHello(configured.clone())
                );
                let expected: Vec<_> = configured
                    .capabilities
                    .iter()
                    .filter(|cap| {
                        cap.name != audio_up::CAPABILITY && cap.name != control::TEXT_CAPABILITY
                    })
                    .cloned()
                    .collect();
                for legacy in [
                    original.intersect(&configured).unwrap(),
                    configured.intersect(&original).unwrap(),
                ] {
                    assert_eq!(legacy.capabilities, expected);
                    assert!(!audio_up::uplink_selected(&legacy.select().unwrap()));
                }
            }
        }
    }
}

#[test]
fn clipboard_duplex_microphone_and_text_complete_the_real_startup() {
    let original = native::control_offer_with_duplex(true, false);
    assert_eq!(original.capabilities.len(), negotiation::MAX_CAPABILITIES);
    let microphone = audio_up::with_microphone(original).unwrap();
    let configured = control::with_committed_text(microphone).unwrap();
    assert_eq!(configured.capabilities.len(), negotiation::MAX_CAPABILITIES);
    assert!(
        configured
            .capabilities
            .iter()
            .all(|cap| cap.name != attachment::BULK_RECORD_CAPABILITY
                && cap.name != attachment::credit::CAPABILITY)
    );
    let mut startup = Startup::new(configured.clone(), 10, 10_000).unwrap();
    assert_eq!(
        negotiation::decode(
            startup.pending(10).unwrap().unwrap(),
            negotiation::MAX_RECORD,
            0
        )
        .unwrap(),
        Message::ClientHello(configured.clone())
    );
    startup.sent(11).unwrap();
    startup
        .receive(
            &bytes(&Message::HostCapabilities(
                configured.intersect(&configured).unwrap(),
            )),
            12,
        )
        .unwrap();
    let Message::SelectedConfiguration(selected) = negotiation::decode(
        startup.pending(13).unwrap().unwrap(),
        negotiation::MAX_RECORD,
        0,
    )
    .unwrap() else {
        panic!("expected the original profile's selection");
    };
    assert_eq!(selected.capabilities, configured.capabilities);
    assert!(audio_up::uplink_selected(&selected));
    assert!(fr_wire::files::duplex_selected(&selected));
    assert!(
        selected
            .capabilities
            .iter()
            .any(|cap| cap.name == attachment::CLIPBOARD_CAPABILITY)
    );
    startup.sent(14).unwrap();
    startup
        .receive(&bytes(&opened(selected.clone())), 15)
        .unwrap();
    startup.bound(7, 16).unwrap();
    startup.sent(17).unwrap();
    assert_eq!(startup.finish(18).unwrap().selection, selected);
}
