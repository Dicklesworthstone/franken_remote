//! Actual shipped host/client profiles and negotiation bytes, without native I/O.
use super::*;
use fr_client::native::{control_offer_with_auxiliary, observation_offer_with_audio};
use fr_wire::negotiation::{self, Message, Selection};

fn present(offer: &Offer, name: &str) -> bool {
    offer.capabilities.iter().any(|cap| cap.name == name)
}
fn legacy(mut offer: Offer) -> Offer {
    offer
        .capabilities
        .retain(|cap| cap.name != attachment::BULK_RECORD_CAPABILITY);
    offer
}
fn round_trip(message: &Message) -> Message {
    let mut bytes = [0_u8; negotiation::MAX_RECORD];
    let n = negotiation::encode(message, bytes.len(), &mut bytes).unwrap();
    assert!(n <= negotiation::MAX_RECORD);
    let decoded = negotiation::decode(&bytes[..n], negotiation::MAX_RECORD, 0).unwrap();
    assert_eq!(&decoded, message);
    decoded
}
fn select(host: &Offer, client: &Offer) -> Selection {
    let Message::ClientHello(client) = round_trip(&Message::ClientHello(client.clone())) else {
        panic!("client hello");
    };
    let intersection = host.intersect(&client).unwrap();
    let Message::HostCapabilities(intersection) =
        round_trip(&Message::HostCapabilities(intersection))
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

#[test]
fn every_host_option_combination_preserves_explicit_opt_in_and_required_gates() {
    for control in [false, true] {
        for clipboard in [false, true] {
            for files in [false, true] {
                for audio in [false, true] {
                    let offer = host_offer_with_files(control, clipboard, audio, files);
                    offer.validate().unwrap();
                    let credit = offer
                        .capabilities
                        .iter()
                        .filter(|cap| cap.name == attachment::credit::CAPABILITY)
                        .collect::<Vec<_>>();
                    assert_eq!(credit.len(), 1);
                    assert_eq!(credit[0].version, attachment::credit::VERSION);
                    assert!(!credit[0].required);
                    assert_eq!(
                        present(&offer, fr_wire::input_ticket::AUTHORITY_CAPABILITY),
                        control
                    );
                    assert_eq!(
                        present(&offer, fr_wire::input_ticket::READINESS_CAPABILITY),
                        control
                    );
                    assert!(offer.capabilities.len() <= negotiation::MAX_CAPABILITIES);
                    assert_eq!(offer.limits, ProtocolLimits::ABSOLUTE);
                    let required: Vec<_> =
                        offer.capabilities.iter().filter(|c| c.required).collect();
                    let baseline = host_offer(control);
                    assert_eq!(
                        required,
                        baseline
                            .capabilities
                            .iter()
                            .filter(|c| c.required)
                            .collect::<Vec<_>>()
                    );
                    assert_eq!(
                        present(&offer, attachment::BULK_RECORD_CAPABILITY),
                        control && (clipboard || files)
                    );
                    for cap in &offer.capabilities {
                        if cap.name == attachment::BULK_RECORD_CAPABILITY {
                            assert_eq!(cap.version, attachment::BULK_RECORD_VERSION);
                            assert!(!cap.required);
                        }
                        if cap.name == fr_wire::input_ticket::AUTHORITY_CAPABILITY {
                            assert_eq!(cap.version, fr_wire::input_ticket::AUTHORITY_VERSION);
                            assert!(!cap.required);
                        }
                        if cap.name == fr_wire::input_ticket::READINESS_CAPABILITY {
                            assert_eq!(cap.version, fr_wire::input_ticket::READINESS_VERSION);
                            assert!(!cap.required);
                        }
                    }
                    for &(name, _) in &CLIPBOARD_CAPABILITIES {
                        assert_eq!(present(&offer, name), control && clipboard);
                    }
                    for &(name, _) in &FILE_CAPABILITIES {
                        assert_eq!(present(&offer, name), control && files);
                    }
                    let observed = select(&offer, &observation_offer_with_audio());
                    assert_eq!(observed.role, Role::Observe);
                    assert!(attachment::credit::selected(&observed));
                    assert!(!attachment::bulk_records_selected(&observed));
                    assert!(!fr_wire::input_ticket::authority_selected(&observed).unwrap());
                    assert_eq!(fr_wire::audio_control::downlink_selected(&observed), audio);
                    if !control {
                        assert_eq!(
                            offer.intersect(&control_offer_with_auxiliary(true, true, true)),
                            Err(negotiation::Error::RequiredCapability)
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn real_host_and_client_round_trip_every_independent_lane_combination() {
    for hc in [false, true] {
        for hf in [false, true] {
            for ha in [false, true] {
                let host = with_line_scroll(host_offer_with_files(true, hc, ha, hf));
                for cc in [false, true] {
                    for cf in [false, true] {
                        for ca in [false, true] {
                            let client = control_offer_with_auxiliary(cc, cf, ca);
                            let selected = select(&host, &client);
                            assert_eq!(selected.role, Role::RequestControl);
                            assert!(attachment::credit::selected(&selected));
                            assert!(fr_wire::input_ticket::authority_selected(&selected).unwrap());
                            assert!(fr_wire::input_ticket::readiness_selected(&selected).unwrap());
                            assert_eq!(selected.limits, ProtocolLimits::ABSOLUTE);
                            assert_eq!(
                                attachment::bulk_records_selected(&selected),
                                (hc || hf) && (cc || cf)
                            );
                            for (caps, enabled) in [
                                (CLIPBOARD_CAPABILITIES, hc && cc),
                                (FILE_CAPABILITIES, hf && cf),
                            ] {
                                for (name, version) in caps {
                                    assert_eq!(
                                        selected
                                            .capabilities
                                            .iter()
                                            .any(|c| c.name == name && c.version == version),
                                        enabled
                                    );
                                }
                            }
                            assert_eq!(
                                fr_wire::audio_control::downlink_selected(&selected),
                                ha && ca
                            );
                            // A shared record-size extension cannot enable an unshared lane.
                            for cap in client.capabilities.iter().filter(|c| c.required) {
                                assert!(selected.capabilities.contains(cap));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn either_legacy_peer_keeps_all_other_selected_capabilities_and_limits() {
    let host = with_line_scroll(host_offer_with_files(true, true, true, true));
    let client = control_offer_with_auxiliary(true, true, true);
    let mut expected = select(&host, &client);
    assert!(attachment::bulk_records_selected(&expected));
    expected
        .capabilities
        .retain(|c| c.name != attachment::BULK_RECORD_CAPABILITY);
    for (host, client) in [
        (legacy(host.clone()), client.clone()),
        (host, legacy(client)),
    ] {
        let selected = select(&host, &client);
        assert_eq!(selected, expected);
        assert!(!attachment::bulk_records_selected(&selected));
    }
}

#[test]
fn unknown_optional_bulk_version_is_not_a_failed_control_or_an_upgrade() {
    let host = host_offer_with_files(true, true, false, true);
    let client = control_offer_with_auxiliary(true, true, false);
    let expected = select(&legacy(host.clone()), &client);
    for change_host in [false, true] {
        let mut h = host.clone();
        let mut c = client.clone();
        let changed = if change_host { &mut h } else { &mut c };
        changed
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == attachment::BULK_RECORD_CAPABILITY)
            .unwrap()
            .version += 1;
        let selected = select(&h, &c);
        assert_eq!(selected, expected);
        assert!(!attachment::bulk_records_selected(&selected));
    }
}

#[test]
fn attachment_credit_negotiates_independently_and_preserves_legacy_peers() {
    for (host, client) in [
        (host_offer(false), fr_client::native::observation_offer()),
        (
            host_offer_with_audio(false, false, true),
            observation_offer_with_audio(),
        ),
        (
            with_line_scroll(host_offer_with_files(true, true, true, true)),
            control_offer_with_auxiliary(true, true, true),
        ),
    ] {
        let mut expected = select(&host, &client);
        assert!(attachment::credit::selected(&expected));
        expected
            .capabilities
            .retain(|cap| cap.name != attachment::credit::CAPABILITY);
        for change_host in [false, true] {
            for omit in [false, true] {
                let mut h = host.clone();
                let mut c = client.clone();
                let changed = if change_host { &mut h } else { &mut c };
                if omit {
                    changed
                        .capabilities
                        .retain(|cap| cap.name != attachment::credit::CAPABILITY);
                } else {
                    changed
                        .capabilities
                        .iter_mut()
                        .find(|cap| cap.name == attachment::credit::CAPABILITY)
                        .unwrap()
                        .version += 1;
                }
                let selected = select(&h, &c);
                assert_eq!(selected, expected);
                assert!(!attachment::credit::selected(&selected));
            }
        }
    }
}

#[test]
fn authority_tickets_require_both_original_peers_without_changing_other_profiles() {
    let host = with_line_scroll(host_offer_with_files(true, true, true, true));
    let client = control_offer_with_auxiliary(true, true, true);
    let mut expected = select(&host, &client);
    assert!(fr_wire::input_ticket::authority_selected(&expected).unwrap());
    expected.capabilities.retain(|cap| {
        cap.name != fr_wire::input_ticket::AUTHORITY_CAPABILITY
            && cap.name != fr_wire::input_ticket::READINESS_CAPABILITY
    });
    for change_host in [false, true] {
        for omit in [false, true] {
            let mut h = host.clone();
            let mut c = client.clone();
            let changed = if change_host { &mut h } else { &mut c };
            // A peer without the authority extension also predates readiness,
            // whose selected profile explicitly depends on authority v1.
            changed
                .capabilities
                .retain(|cap| cap.name != fr_wire::input_ticket::READINESS_CAPABILITY);
            if omit {
                changed
                    .capabilities
                    .retain(|cap| cap.name != fr_wire::input_ticket::AUTHORITY_CAPABILITY);
            } else {
                changed
                    .capabilities
                    .iter_mut()
                    .find(|cap| cap.name == fr_wire::input_ticket::AUTHORITY_CAPABILITY)
                    .unwrap()
                    .version += 1;
            }
            let selected = select(&h, &c);
            assert_eq!(selected, expected);
            assert!(!fr_wire::input_ticket::authority_selected(&selected).unwrap());
            assert!(attachment::bulk_records_selected(&selected));
        }
    }
}

#[test]
fn readiness_tickets_require_both_peers_and_keep_authority_only_compatibility() {
    use fr_wire::input_ticket::{self, Profile};
    let host = with_line_scroll(host_offer_with_files(true, true, true, true));
    let client = control_offer_with_auxiliary(true, true, true);
    let mut expected = select(&host, &client);
    assert_eq!(
        input_ticket::selected_profile(&expected),
        Ok(Profile::Readiness)
    );
    for offer in [&host, &client] {
        assert!(offer.capabilities.len() <= negotiation::MAX_CAPABILITIES);
        let readiness = offer
            .capabilities
            .iter()
            .find(|cap| cap.name == input_ticket::READINESS_CAPABILITY)
            .unwrap();
        assert!(!readiness.required);
        assert_eq!(readiness.version, input_ticket::READINESS_VERSION);
    }
    expected
        .capabilities
        .retain(|cap| cap.name != input_ticket::READINESS_CAPABILITY);
    for change_host in [false, true] {
        for omit in [false, true] {
            let mut h = host.clone();
            let mut c = client.clone();
            let changed = if change_host { &mut h } else { &mut c };
            if omit {
                changed
                    .capabilities
                    .retain(|cap| cap.name != input_ticket::READINESS_CAPABILITY);
            } else {
                changed
                    .capabilities
                    .iter_mut()
                    .find(|cap| cap.name == input_ticket::READINESS_CAPABILITY)
                    .unwrap()
                    .version += 1;
            }
            let selected = select(&h, &c);
            assert_eq!(selected, expected);
            assert_eq!(
                input_ticket::selected_profile(&selected),
                Ok(Profile::Authority)
            );
            assert!(attachment::bulk_records_selected(&selected));
        }
    }
    let observer = select(&host, &fr_client::native::observation_offer());
    assert_eq!(
        input_ticket::selected_profile(&observer),
        Ok(Profile::Legacy)
    );
}

#[test]
fn downloads_negotiate_direction_without_advertising_a_legacy_upload_directory() {
    for clipboard in [false, true] {
        for audio in [false, true] {
            let host = with_line_scroll(host_offer_with_download(true, clipboard, audio, true));
            let client = fr_client::native::control_offer_with_download(clipboard, audio);
            for offer in [&host, &client] {
                offer.validate().unwrap();
                assert!(offer.capabilities.len() <= negotiation::MAX_CAPABILITIES);
                assert!(!present(offer, fr_wire::files::CHANNEL_SCOPE_CAPABILITY));
                assert!(present(offer, fr_wire::files::DOWNLOAD_CAPABILITY));
            }
            let selected = select(&host, &client);
            super::super::file_download_lane(&selected).unwrap();
            assert!(super::super::files_lane(&selected).is_err());
            assert_eq!(fr_wire::audio_control::downlink_selected(&selected), audio);
            for &(name, version) in &CLIPBOARD_CAPABILITIES {
                assert_eq!(
                    selected
                        .capabilities
                        .iter()
                        .any(|c| c.name == name && c.version == version),
                    clipboard
                );
            }
            let old_client = control_offer_with_auxiliary(clipboard, true, audio);
            let old_host = host_offer_with_files(true, clipboard, audio, true);
            for selected in [select(&host, &old_client), select(&old_host, &client)] {
                assert_eq!(selected.role, Role::RequestControl);
                assert!(super::super::file_download_lane(&selected).is_err());
                assert!(super::super::files_lane(&selected).is_err());
            }
            let disabled = host_offer_with_download(true, clipboard, audio, false);
            assert!(!present(&disabled, fr_wire::files::DOWNLOAD_CAPABILITY));
            assert!(!present(
                &disabled,
                fr_wire::files::CHANNEL_SCOPE_CAPABILITY
            ));
            let observing = select(&host, &fr_client::native::observation_offer());
            assert!(super::super::file_download_lane(&observing).is_err());
        }
    }
}

#[test]
fn duplex_profiles_round_trip_with_independent_optional_lanes_and_original_bounds() {
    for clipboard in [false, true] {
        for audio in [false, true] {
            let host = with_line_scroll(host_offer_with_duplex(true, clipboard, audio));
            let client = fr_client::native::control_offer_with_duplex(clipboard, audio);
            for offer in [&host, &client] {
                offer.validate().unwrap();
                assert!(offer.capabilities.len() <= negotiation::MAX_CAPABILITIES);
                assert_eq!(offer.limits, ProtocolLimits::ABSOLUTE);
                for name in [
                    fr_wire::files::CAPABILITY,
                    fr_wire::files::DOWNLOAD_CAPABILITY,
                    fr_wire::files::CHANNEL_SCOPE_CAPABILITY,
                    fr_wire::files::DUPLEX_CAPABILITY,
                    attachment::FILES_CAPABILITY,
                    attachment::INPUT_CAPABILITY,
                    fr_wire::input_ticket::AUTHORITY_CAPABILITY,
                    fr_wire::input_ticket::READINESS_CAPABILITY,
                    fr_wire::cursor::CAPABILITY,
                    fr_wire::recovery_request::CAPABILITY,
                    control::LINE_SCROLL_CAPABILITY,
                ] {
                    assert!(present(offer, name), "missing {name}");
                }
                // The fullest profile uses the already bounded fixed allowance
                // path instead of raising the negotiation count or dropping gates.
                assert_eq!(
                    present(offer, attachment::credit::CAPABILITY),
                    !(clipboard && audio)
                );
            }
            let selected = select(&host, &client);
            assert!(fr_wire::files::duplex_selected(&selected));
            super::super::files_lane(&selected).unwrap();
            super::super::file_download_lane(&selected).unwrap();
            assert_eq!(fr_wire::audio_control::downlink_selected(&selected), audio);
            for &(name, version) in &CLIPBOARD_CAPABILITIES {
                assert_eq!(
                    selected
                        .capabilities
                        .iter()
                        .any(|c| c.name == name && c.version == version),
                    clipboard
                );
            }
            let observation = select(&host, &fr_client::native::observation_offer());
            assert!(!fr_wire::files::duplex_selected(&observation));
            assert!(super::super::files_lane(&observation).is_err());
            assert!(super::super::file_download_lane(&observation).is_err());
        }
    }
}

#[test]
fn duplex_with_each_legacy_peer_selects_only_that_peers_explicit_direction() {
    let host = with_line_scroll(host_offer_with_duplex(true, true, true));
    let client = fr_client::native::control_offer_with_duplex(true, true);
    for selected in [
        select(&host, &control_offer_with_auxiliary(true, true, true)),
        select(
            &with_line_scroll(host_offer_with_files(true, true, true, true)),
            &client,
        ),
    ] {
        assert!(!fr_wire::files::duplex_selected(&selected));
        assert_eq!(selected.role, Role::RequestControl);
        super::super::files_lane(&selected).unwrap();
        assert!(super::super::file_download_lane(&selected).is_err());
    }
    for selected in [
        select(
            &host,
            &fr_client::native::control_offer_with_download(true, true),
        ),
        select(
            &with_line_scroll(host_offer_with_download(true, true, true, true)),
            &client,
        ),
    ] {
        assert!(!fr_wire::files::duplex_selected(&selected));
        assert_eq!(selected.role, Role::RequestControl);
        super::super::file_download_lane(&selected).unwrap();
        assert!(super::super::files_lane(&selected).is_err());
    }
    // Both old direction capabilities alone are ambiguous, not consent to a
    // second owner. Omission or an unknown duplex version refuses both lanes.
    for version in [None, Some(fr_wire::files::DUPLEX_VERSION + 1)] {
        let mut peer = client.clone();
        if let Some(version) = version {
            peer.capabilities
                .iter_mut()
                .find(|c| c.name == fr_wire::files::DUPLEX_CAPABILITY)
                .unwrap()
                .version = version;
        } else {
            peer.capabilities
                .retain(|c| c.name != fr_wire::files::DUPLEX_CAPABILITY);
        }
        let selected = select(&host, &peer);
        assert_eq!(selected.role, Role::RequestControl);
        assert!(!fr_wire::files::duplex_selected(&selected));
        assert!(super::super::files_lane(&selected).is_err());
        assert!(super::super::file_download_lane(&selected).is_err());
    }
    assert!(!present(
        &host_offer_with_duplex(false, true, true),
        fr_wire::files::DUPLEX_CAPABILITY
    ));
}
