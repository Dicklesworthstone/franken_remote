//! Actual shipped offers and their bilateral negotiation, without native I/O.
use fr_client::native::{
    control_offer_with_auxiliary, observation_offer, observation_offer_with_audio,
};
use fr_wire::{
    attachment::credit,
    negotiation::{MAX_CAPABILITIES, Offer, Role},
};

fn without_credit(mut offer: Offer) -> Offer {
    offer
        .capabilities
        .retain(|cap| cap.name != credit::CAPABILITY);
    offer
}

#[test]
fn every_native_profile_offers_one_optional_attachment_credit_boundary() {
    let mut offers = vec![observation_offer(), observation_offer_with_audio()];
    for clipboard in [false, true] {
        for files in [false, true] {
            for audio in [false, true] {
                offers.push(control_offer_with_auxiliary(clipboard, files, audio));
            }
        }
    }
    for offer in offers {
        offer.validate().unwrap();
        assert!(offer.capabilities.len() <= MAX_CAPABILITIES);
        let caps: Vec<_> = offer
            .capabilities
            .iter()
            .filter(|cap| cap.name == credit::CAPABILITY)
            .collect();
        assert_eq!(caps.len(), 1);
        assert_eq!(caps[0].version, credit::VERSION);
        assert!(!caps[0].required);
        assert!(credit::selected(&offer.select().unwrap()));
    }
}

#[test]
fn either_legacy_peer_or_an_unknown_version_keeps_every_other_capability() {
    for client in [
        observation_offer(),
        observation_offer_with_audio(),
        control_offer_with_auxiliary(true, true, true),
    ] {
        let mut host = client.clone();
        host.role = Role::Observe;
        let selected = host.intersect(&client).unwrap().select().unwrap();
        assert!(credit::selected(&selected));
        for change_host in [false, true] {
            for remove in [false, true] {
                let mut h = host.clone();
                let mut c = client.clone();
                let changed = if change_host { &mut h } else { &mut c };
                if remove {
                    *changed = without_credit(changed.clone());
                } else {
                    changed
                        .capabilities
                        .iter_mut()
                        .find(|cap| cap.name == credit::CAPABILITY)
                        .unwrap()
                        .version += 1;
                }
                let intersection = h.intersect(&c).unwrap();
                c.check_host(&intersection).unwrap();
                let legacy = intersection.select().unwrap();
                legacy.check_against(&c).unwrap();
                assert!(!credit::selected(&legacy));
                assert_eq!(legacy.role, selected.role);
                assert_eq!(legacy.limits, selected.limits);
                assert_eq!(
                    legacy.capabilities,
                    without_credit(client.clone()).capabilities
                );
            }
        }
    }
}
