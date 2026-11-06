//! The actual native constructors and bilateral startup intersection.
use fr_client::native::*;
use fr_wire::{
    attachment,
    negotiation::{MAX_CAPABILITIES, Offer, Role},
    quality_decision, receiver_metrics,
};

fn without_quality(mut offer: Offer) -> Offer {
    offer
        .capabilities
        .retain(|cap| cap.name != quality_decision::CAPABILITY);
    offer
}

#[test]
fn every_runtime_profile_reports_quality_only_when_an_existing_slot_is_free() {
    let mut offers = vec![
        (observation_offer(), true),
        (observation_offer_with_audio(), true),
        (control_offer(), true),
        (control_offer_with_clipboard(), true),
        (control_offer_with_files(), true),
        (control_offer_with_clipboard_and_files(), true),
    ];
    for clipboard in [false, true] {
        for audio in [false, true] {
            offers.push((
                control_offer_with_download(clipboard, audio),
                !(clipboard && audio),
            ));
            offers.push((control_offer_with_duplex(clipboard, audio), !clipboard));
            for files in [false, true] {
                offers.push((
                    control_offer_with_auxiliary(clipboard, files, audio),
                    !(clipboard && files && audio),
                ));
            }
        }
    }
    for (offer, expected) in offers {
        offer.validate().unwrap();
        assert!(offer.capabilities.len() <= MAX_CAPABILITIES);
        let caps: Vec<_> = offer
            .capabilities
            .iter()
            .filter(|cap| cap.name == quality_decision::CAPABILITY)
            .collect();
        assert_eq!(caps.len(), usize::from(expected));
        for cap in caps {
            assert_eq!(cap.version, quality_decision::VERSION);
            assert!(!cap.required);
        }
        assert_eq!(
            quality_decision::selected(&offer.select().unwrap()),
            expected
        );
    }
    inspection_offer().unwrap().validate().unwrap();
}

#[test]
fn at_capacity_reporting_drops_before_bulk_credit_or_decoder_feedback() {
    for offer in [
        control_offer_with_auxiliary(true, true, true),
        control_offer_with_download(true, true),
        control_offer_with_duplex(true, false),
    ] {
        assert_eq!(offer.capabilities.len(), MAX_CAPABILITIES);
        assert!(!quality_decision::selected(&offer.select().unwrap()));
        for preserved in [
            attachment::BULK_RECORD_CAPABILITY,
            attachment::credit::CAPABILITY,
            receiver_metrics::CAPABILITY,
        ] {
            assert!(
                offer.capabilities.iter().any(|cap| cap.name == preserved),
                "{preserved}"
            );
        }
    }
    let fullest = control_offer_with_duplex(true, true);
    assert_eq!(fullest.capabilities.len(), MAX_CAPABILITIES);
    assert!(!quality_decision::selected(&fullest.select().unwrap()));
    assert!(!attachment::bulk_records_selected(
        &fullest.select().unwrap()
    ));
    assert!(!attachment::credit::selected(&fullest.select().unwrap()));
    assert!(
        fullest
            .capabilities
            .iter()
            .any(|cap| cap.name == receiver_metrics::CAPABILITY)
    );
}

#[test]
fn legacy_or_unknown_version_intersections_preserve_all_existing_capabilities() {
    for client in [
        observation_offer(),
        control_offer(),
        control_offer_with_clipboard_and_files(),
        control_offer_with_duplex(false, true),
    ] {
        let mut host = client.clone();
        host.role = Role::Observe;
        let selected = host.intersect(&client).unwrap().select().unwrap();
        assert!(quality_decision::selected(&selected));
        for change_host in [false, true] {
            for remove in [false, true] {
                let mut h = host.clone();
                let mut c = client.clone();
                let changed = if change_host { &mut h } else { &mut c };
                if remove {
                    *changed = without_quality(changed.clone());
                } else {
                    changed
                        .capabilities
                        .iter_mut()
                        .find(|cap| cap.name == quality_decision::CAPABILITY)
                        .unwrap()
                        .version += 1;
                }
                let intersection = h.intersect(&c).unwrap();
                c.check_host(&intersection).unwrap();
                let legacy = intersection.select().unwrap();
                legacy.check_against(&c).unwrap();
                assert!(!quality_decision::selected(&legacy));
                assert_eq!(legacy.role, selected.role);
                assert_eq!(legacy.limits, selected.limits);
                assert_eq!(
                    legacy.capabilities,
                    without_quality(client.clone()).capabilities
                );
            }
        }
    }
}
