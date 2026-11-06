//! Real shipped offer builders and negotiation bytes, without native I/O.
#![cfg(target_os = "linux")]

use fr_client::native;
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    attachment,
    negotiation::{self, Capability, Message, Offer, Role, Selection},
    quality_decision, receiver_metrics,
};
use frd::session_startup::{
    host_offer, host_offer_with_download, host_offer_with_duplex, host_offer_with_files,
    with_line_scroll, with_receiver_metrics,
};

fn present(offer: &Offer, name: &str) -> bool {
    offer.capabilities.iter().any(|cap| cap.name == name)
}

fn round_trip(message: &Message) -> Message {
    let mut bytes = [0; negotiation::MAX_RECORD];
    let length = negotiation::encode(message, bytes.len(), &mut bytes).unwrap();
    let decoded = negotiation::decode(&bytes[..length], bytes.len(), 0).unwrap();
    assert_eq!(&decoded, message);
    decoded
}

fn select(host: &Offer, client: &Offer) -> Selection {
    let Message::ClientHello(client) = round_trip(&Message::ClientHello(client.clone())) else {
        panic!("client hello");
    };
    let Message::HostCapabilities(intersection) =
        round_trip(&Message::HostCapabilities(host.intersect(&client).unwrap()))
    else {
        panic!("host capabilities");
    };
    client.check_host(&intersection).unwrap();
    let Message::SelectedConfiguration(selection) = round_trip(&Message::SelectedConfiguration(
        intersection.select().unwrap(),
    )) else {
        panic!("selected configuration");
    };
    selection.check_against(&client).unwrap();
    selection
}

fn has_metrics(selection: &Selection) -> bool {
    selection.capabilities.iter().any(|cap| {
        cap.name == receiver_metrics::CAPABILITY
            && cap.version == receiver_metrics::VERSION
            && !cap.required
    })
}

fn assert_preserved(original: &Offer, adaptive: &Offer) {
    assert_eq!(adaptive.versions, original.versions);
    assert_eq!(adaptive.profile, original.profile);
    assert_eq!(adaptive.profile_version, original.profile_version);
    assert_eq!(adaptive.role, original.role);
    assert_eq!(adaptive.limits, original.limits);
    assert!(adaptive.capabilities.len() <= negotiation::MAX_CAPABILITIES);
    for cap in &original.capabilities {
        if !adaptive.capabilities.contains(cap) {
            assert_eq!(original.capabilities.len(), negotiation::MAX_CAPABILITIES);
            assert_eq!(cap.name, attachment::BULK_RECORD_CAPABILITY);
            assert!(!cap.required);
        }
    }
    assert!(adaptive.capabilities.iter().any(|cap| {
        cap.name == receiver_metrics::CAPABILITY
            && cap.version == receiver_metrics::VERSION
            && !cap.required
    }));
    adaptive.validate().unwrap();
}

#[test]
fn adaptive_observation_is_optional_and_the_default_offer_does_not_request_metrics() {
    for control in [false, true] {
        for audio in [false, true] {
            let host = host_offer_with_files(control, false, audio, false);
            let client = if audio {
                native::observation_offer_with_audio()
            } else {
                native::observation_offer()
            };
            assert!(!present(&host, receiver_metrics::CAPABILITY));
            assert!(!has_metrics(&select(&host, &client)));
            let adaptive = with_receiver_metrics(host.clone()).unwrap();
            assert_preserved(&host, &adaptive);
            let selected = select(&adaptive, &client);
            assert_eq!(selected.role, Role::Observe);
            assert!(has_metrics(&selected));
            assert_eq!(selected.limits, ProtocolLimits::ABSOLUTE);
            assert_eq!(fr_wire::audio_control::downlink_selected(&selected), audio);
        }
    }
}

#[test]
fn every_control_auxiliary_and_file_direction_profile_retains_negotiated_metrics() {
    for clipboard in [false, true] {
        for audio in [false, true] {
            let hosts = [
                host_offer_with_files(true, clipboard, audio, false),
                host_offer_with_files(true, clipboard, audio, true),
                host_offer_with_download(true, clipboard, audio, true),
                host_offer_with_duplex(true, clipboard, audio),
            ];
            let clients = [
                native::control_offer_with_auxiliary(clipboard, false, audio),
                native::control_offer_with_auxiliary(clipboard, true, audio),
                native::control_offer_with_download(clipboard, audio),
                native::control_offer_with_duplex(clipboard, audio),
            ];
            for (original, client) in hosts.into_iter().zip(clients) {
                let original = with_line_scroll(original);
                let host = with_receiver_metrics(original.clone()).unwrap();
                assert_preserved(&original, &host);
                client.validate().unwrap();
                assert!(present(&client, receiver_metrics::CAPABILITY));
                for cap in native::control_offer().capabilities {
                    // The quality advisory is a diagnostic. A full offer omits
                    // it before any older feature, and only a full one does.
                    if cap.name == quality_decision::CAPABILITY
                        && client.capabilities.len() == negotiation::MAX_CAPABILITIES
                    {
                        continue;
                    }
                    // The full duplex profile already used fixed bounded credit;
                    // every authority, privacy, cursor and recovery gate remains.
                    if cap.name != attachment::credit::CAPABILITY {
                        assert!(client.capabilities.contains(&cap), "{}", cap.name);
                    }
                }
                let selected = select(&host, &client);
                assert_eq!(selected.role, Role::RequestControl);
                assert!(has_metrics(&selected));
                assert_eq!(selected.limits, ProtocolLimits::ABSOLUTE);
                assert!(fr_wire::input_ticket::authority_selected(&selected).unwrap());
                assert!(fr_wire::input_ticket::readiness_selected(&selected).unwrap());
                assert_eq!(fr_wire::audio_control::downlink_selected(&selected), audio);
                let without_metrics = |mut selection: Selection| {
                    selection
                        .capabilities
                        .retain(|cap| cap.name != receiver_metrics::CAPABILITY);
                    selection
                };
                // Adding feedback does not discard a previously selected feature
                // except the explicitly lower-priority record-sizing extension.
                let mut expected = without_metrics(select(&original, &client));
                if !attachment::bulk_records_selected(&selected) {
                    expected
                        .capabilities
                        .retain(|cap| cap.name != attachment::BULK_RECORD_CAPABILITY);
                }
                assert_eq!(without_metrics(selected), expected);
            }
        }
    }
}

#[test]
fn full_duplex_prioritizes_feedback_without_expanding_any_existing_bound() {
    let original = with_line_scroll(host_offer_with_duplex(true, true, true));
    assert_eq!(original.capabilities.len(), negotiation::MAX_CAPABILITIES);
    assert!(present(&original, attachment::BULK_RECORD_CAPABILITY));
    let host = with_receiver_metrics(original.clone()).unwrap();
    let client = native::control_offer_with_duplex(true, true);
    for offer in [&host, &client] {
        assert_eq!(offer.capabilities.len(), negotiation::MAX_CAPABILITIES);
        assert!(!present(offer, attachment::BULK_RECORD_CAPABILITY));
        // This fixed-allowance fallback predates adaptive feedback.
        assert!(!present(offer, attachment::credit::CAPABILITY));
        assert!(present(offer, receiver_metrics::CAPABILITY));
        assert_eq!(offer.limits, ProtocolLimits::ABSOLUTE);
    }
    assert_preserved(&original, &host);
    let selected = select(&host, &client);
    assert!(has_metrics(&selected));
    assert!(fr_wire::files::duplex_selected(&selected));
    assert!(fr_wire::audio_control::downlink_selected(&selected));
    assert!(!attachment::bulk_records_selected(&selected));
}

#[test]
fn legacy_viewers_and_incompatible_optional_versions_keep_feedback_unselected() {
    let host = with_receiver_metrics(with_line_scroll(host_offer(true))).unwrap();
    let client = native::control_offer();
    let mut expected = select(&host, &client);
    expected
        .capabilities
        .retain(|cap| cap.name != receiver_metrics::CAPABILITY);
    let mut legacy = client.clone();
    legacy
        .capabilities
        .retain(|cap| cap.name != receiver_metrics::CAPABILITY);
    let mut newer = client;
    newer
        .capabilities
        .iter_mut()
        .find(|cap| cap.name == receiver_metrics::CAPABILITY)
        .unwrap()
        .version += 1;
    for peer in [legacy, newer] {
        let selected = select(&host, &peer);
        assert!(!has_metrics(&selected));
        assert_eq!(selected, expected);
    }
}

fn fill(mut offer: Offer) -> Offer {
    while offer.capabilities.len() < negotiation::MAX_CAPABILITIES {
        offer.capabilities.push(Capability {
            name: format!("test-cap-{}", offer.capabilities.len()),
            version: 1,
            required: false,
        });
    }
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer.validate().unwrap();
    offer
}

#[test]
fn extension_is_idempotent_and_capacity_failure_cannot_remove_required_capabilities() {
    let adaptive = with_receiver_metrics(host_offer(true)).unwrap();
    assert_eq!(with_receiver_metrics(adaptive.clone()).unwrap(), adaptive);
    let full = fill(host_offer(true));
    assert_eq!(
        with_receiver_metrics(full),
        Err(negotiation::Error::Invalid)
    );
    let mut required_bulk = fill(host_offer_with_files(true, true, false, false));
    required_bulk
        .capabilities
        .iter_mut()
        .find(|cap| cap.name == attachment::BULK_RECORD_CAPABILITY)
        .unwrap()
        .required = true;
    assert_eq!(
        with_receiver_metrics(required_bulk),
        Err(negotiation::Error::Invalid)
    );
    for incompatible in [true, false] {
        let mut invalid = adaptive.clone();
        let cap = invalid
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == receiver_metrics::CAPABILITY)
            .unwrap();
        if incompatible {
            cap.version += 1;
        } else {
            cap.required = true;
        }
        assert_eq!(
            with_receiver_metrics(invalid),
            Err(negotiation::Error::RequiredCapability)
        );
    }
}

#[test]
fn terminal_inspection_keeps_its_required_marker_without_requesting_decoder_load() {
    let inspection = native::inspection_offer().unwrap();
    inspection.validate().unwrap();
    assert_eq!(inspection.capabilities.len(), negotiation::MAX_CAPABILITIES);
    assert_eq!(inspection.role, Role::Observe);
    assert!(fr_wire::inspection::requested(&inspection));
    assert!(!present(&inspection, receiver_metrics::CAPABILITY));
    assert!(present(
        &native::control_offer_with_duplex(true, true),
        receiver_metrics::CAPABILITY
    ));
    let host =
        with_receiver_metrics(with_line_scroll(host_offer_with_duplex(true, true, true))).unwrap();
    let inspected = fr_wire::inspection::intersect(&host, &inspection).unwrap();
    assert!(fr_wire::inspection::requested(&inspected));
    assert!(!present(&inspected, receiver_metrics::CAPABILITY));
}
