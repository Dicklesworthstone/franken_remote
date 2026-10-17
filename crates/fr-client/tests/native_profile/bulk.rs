//! Capability negotiation with real offers/codecs; not native transport proof.
use fr_client::native::*;
use fr_wire::{
    attachment,
    negotiation::{self, Offer, Role},
};

fn without_bulk(mut offer: Offer) -> Offer {
    offer
        .capabilities
        .retain(|c| c.name != attachment::BULK_RECORD_CAPABILITY);
    offer
}

#[test]
fn bulk_requires_positive_selection_and_preserves_both_legacy_peer_directions() {
    for client in [
        control_offer_with_clipboard(),
        control_offer_with_files(),
        control_offer_with_clipboard_and_files(),
    ] {
        let mut host = client.clone();
        host.role = Role::Observe;
        let both = host.intersect(&client).unwrap();
        client.check_host(&both).unwrap();
        let selected = both.select().unwrap();
        assert!(attachment::bulk_records_selected(&selected));
        assert_eq!(selected.role, Role::RequestControl);
        assert_eq!(selected.limits, client.limits);
        for (h, c) in [
            (without_bulk(host.clone()), client.clone()),
            (host.clone(), without_bulk(client.clone())),
        ] {
            let intersection = h.intersect(&c).unwrap();
            c.check_host(&intersection).unwrap();
            let legacy = intersection.select().unwrap();
            legacy.check_against(&c).unwrap();
            assert!(!attachment::bulk_records_selected(&legacy));
            assert_eq!(legacy.role, Role::RequestControl);
            assert_eq!(legacy.limits, selected.limits);
            assert_eq!(legacy.capabilities, without_bulk(both.clone()).capabilities);
        }
    }
}

#[test]
fn observation_plain_control_and_audio_only_do_not_advertise_bulk_records() {
    for offer in [
        observation_offer(),
        observation_offer_with_audio(),
        control_offer(),
        control_offer_with_auxiliary(false, false, true),
    ] {
        offer.validate().unwrap();
        assert!(
            offer
                .capabilities
                .iter()
                .all(|c| c.name != attachment::BULK_RECORD_CAPABILITY)
        );
        assert!(!attachment::bulk_records_selected(&offer.select().unwrap()));
    }
}

#[test]
fn every_auxiliary_combination_has_one_optional_opt_in_and_unchanged_control_gates() {
    let control = control_offer();
    let required: Vec<_> = control
        .capabilities
        .iter()
        .filter(|c| c.required)
        .cloned()
        .collect();
    for clipboard in [false, true] {
        for files in [false, true] {
            for audio in [false, true] {
                let offer = control_offer_with_auxiliary(clipboard, files, audio);
                offer.validate().unwrap();
                assert!(offer.capabilities.len() <= negotiation::MAX_CAPABILITIES);
                assert_eq!(offer.limits, control.limits);
                assert_eq!(
                    offer
                        .capabilities
                        .iter()
                        .filter(|c| c.required)
                        .cloned()
                        .collect::<Vec<_>>(),
                    required
                );
                let bulk: Vec<_> = offer
                    .capabilities
                    .iter()
                    .filter(|c| c.name == attachment::BULK_RECORD_CAPABILITY)
                    .collect();
                assert_eq!(bulk.len(), usize::from(clipboard || files));
                for cap in bulk {
                    assert!(!cap.required);
                    assert_eq!(cap.version, attachment::BULK_RECORD_VERSION);
                }
                assert_eq!(
                    attachment::bulk_records_selected(&offer.select().unwrap()),
                    clipboard || files
                );
            }
        }
    }
}

#[test]
fn an_unknown_optional_bulk_version_drops_only_the_extension() {
    let client = control_offer_with_clipboard_and_files();
    let mut host = client.clone();
    host.role = Role::Observe;
    host.capabilities
        .iter_mut()
        .find(|c| c.name == attachment::BULK_RECORD_CAPABILITY)
        .unwrap()
        .version += 1;
    let intersection = host.intersect(&client).unwrap();
    client.check_host(&intersection).unwrap();
    let selected = intersection.select().unwrap();
    assert!(!attachment::bulk_records_selected(&selected));
    assert_eq!(selected.capabilities, without_bulk(client).capabilities);
}
