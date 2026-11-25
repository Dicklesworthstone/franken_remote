//! Explicit sync selection preserves existing native control and download gates.
use fr_client::native::{self, with_folder_sync};
use fr_wire::{attachment, negotiation, sync_job_state};

#[test]
fn folder_sync_is_explicit_download_only_and_bounded_for_every_auxiliary_profile() {
    for clipboard in [false, true] {
        for audio in [false, true] {
            for original in [
                native::control_offer_with_download(clipboard, audio),
                native::control_offer_with_duplex(clipboard, audio),
            ] {
                assert!(
                    !original
                        .capabilities
                        .iter()
                        .any(|cap| cap.name == sync_job_state::CAPABILITY)
                );
                let sync = with_folder_sync(original.clone()).unwrap_or_else(|error| {
                    panic!(
                        "sync profile clipboard={clipboard}, audio={audio}, duplex={}: {error:?}",
                        original
                            .capabilities
                            .iter()
                            .any(|cap| cap.name == fr_wire::files::DUPLEX_CAPABILITY)
                    )
                });
                assert!(sync.capabilities.len() <= negotiation::MAX_CAPABILITIES);
                assert!(sync_job_state::selected(&sync.select().unwrap()));
                assert_eq!(with_folder_sync(sync.clone()).unwrap(), sync);
                for required in original.capabilities.iter().filter(|cap| cap.required) {
                    assert!(sync.capabilities.contains(required));
                }
                assert_eq!(sync.limits, original.limits);
                assert_eq!(
                    sync.capabilities
                        .iter()
                        .any(|cap| cap.name == fr_wire::files::DUPLEX_CAPABILITY),
                    original
                        .capabilities
                        .iter()
                        .any(|cap| cap.name == fr_wire::files::DUPLEX_CAPABILITY)
                );
            }
        }
    }
    for ordinary in [
        native::observation_offer(),
        native::control_offer(),
        native::control_offer_with_files(),
    ] {
        assert!(with_folder_sync(ordinary).is_err());
    }
}

#[test]
fn fullest_sync_profile_only_yields_optional_decoder_load_reporting() {
    let original = native::control_offer_with_duplex(true, true);
    assert_eq!(original.capabilities.len(), negotiation::MAX_CAPABILITIES);
    let sync = with_folder_sync(original.clone()).unwrap();
    assert_eq!(sync.capabilities.len(), negotiation::MAX_CAPABILITIES);
    assert!(
        !sync
            .capabilities
            .iter()
            .any(|cap| cap.name == fr_wire::receiver_metrics::CAPABILITY)
    );
    for preserved in original
        .capabilities
        .iter()
        .filter(|cap| cap.name != fr_wire::receiver_metrics::CAPABILITY)
    {
        assert!(sync.capabilities.contains(preserved), "{}", preserved.name);
    }
    assert!(sync_job_state::selected(&sync.select().unwrap()));
    assert!(fr_wire::files::duplex_selected(&sync.select().unwrap()));
    let mut required = original.clone();
    required
        .capabilities
        .iter_mut()
        .find(|cap| cap.name == fr_wire::receiver_metrics::CAPABILITY)
        .unwrap()
        .required = true;
    assert!(with_folder_sync(required).is_err());

    // Older peers retain the original media/input/auxiliary profile without
    // either optional sync records or decoder-load reports being emitted.
    let expected: Vec<_> = original
        .capabilities
        .iter()
        .filter(|cap| cap.name != fr_wire::receiver_metrics::CAPABILITY)
        .cloned()
        .collect();
    for intersection in [
        original.intersect(&sync).unwrap(),
        sync.intersect(&original).unwrap(),
    ] {
        assert_eq!(intersection.capabilities, expected);
        assert!(!sync_job_state::selected(&intersection.select().unwrap()));
        assert!(fr_wire::files::duplex_selected(
            &intersection.select().unwrap()
        ));
    }
}

#[test]
fn incompatible_or_full_required_profiles_cannot_silently_drop_authority_or_enable_sync() {
    let base = native::control_offer_with_download(false, false);
    let mut incompatible = with_folder_sync(base.clone()).unwrap();
    incompatible
        .capabilities
        .iter_mut()
        .find(|cap| cap.name == sync_job_state::CAPABILITY)
        .unwrap()
        .version += 1;
    assert!(with_folder_sync(incompatible).is_err());
    let mut full = base;
    for cap in &mut full.capabilities {
        if matches!(
            cap.name.as_str(),
            attachment::BULK_RECORD_CAPABILITY
                | attachment::credit::CAPABILITY
                | fr_wire::quality_decision::CAPABILITY
                | fr_wire::receiver_metrics::CAPABILITY
        ) {
            cap.required = true;
        }
    }
    while full.capabilities.len() < negotiation::MAX_CAPABILITIES {
        full.capabilities.push(negotiation::Capability {
            name: format!("zz-fixture-{}", full.capabilities.len()),
            version: 1,
            required: true,
        });
    }
    full.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    full.validate().unwrap();
    assert!(with_folder_sync(full).is_err());
}

fn extended_upload_profile(options: u8) -> Result<negotiation::Offer, negotiation::Error> {
    let clipboard = options & 1 != 0;
    let audio = options & 2 != 0;
    let mut offer = if options & 4 != 0 {
        native::control_offer_with_duplex(clipboard, audio)
    } else {
        native::control_offer_with_auxiliary(clipboard, true, audio)
    };
    if options & 8 != 0 {
        offer = fr_wire::audio_up::with_microphone(offer)?;
    }
    if options & 16 != 0 {
        offer = fr_wire::control::with_pixel_scroll(offer)?;
    }
    if options & 32 != 0 {
        offer = fr_wire::control::with_committed_text(offer)?;
    }
    if options & 64 != 0 {
        offer = fr_wire::display::picture::with_capability(offer)?;
    }
    offer.validate()?;
    Ok(offer)
}

fn optional_sync_slot(cap: &negotiation::Capability) -> bool {
    !cap.required
        && [
            (
                fr_wire::quality_decision::CAPABILITY,
                fr_wire::quality_decision::VERSION,
            ),
            (
                attachment::BULK_RECORD_CAPABILITY,
                attachment::BULK_RECORD_VERSION,
            ),
            (attachment::credit::CAPABILITY, attachment::credit::VERSION),
            (
                fr_wire::receiver_metrics::CAPABILITY,
                fr_wire::receiver_metrics::VERSION,
            ),
        ]
        .iter()
        .any(|&(name, version)| cap.name == name && cap.version == version)
}

#[test]
fn upload_sync_preserves_every_available_native_auxiliary_and_input_profile() {
    for options in 0u8..128 {
        let Ok(original) = extended_upload_profile(options) else {
            // This exact feature-only combination already exceeds its bounds.
            continue;
        };
        let fits = original.capabilities.len() < negotiation::MAX_CAPABILITIES
            || original.capabilities.iter().any(optional_sync_slot);
        let sync = native::with_folder_sync_upload(original.clone());
        if !fits {
            assert!(sync.is_err(), "no feature may be discarded to make room");
            continue;
        }
        let sync = sync.unwrap();
        let selected = sync.select().unwrap();
        assert!(sync_job_state::upload_selected(&selected));
        assert!(!sync_job_state::selected(&selected));
        assert_eq!(sync.limits, original.limits);
        assert!(sync.capabilities.len() <= negotiation::MAX_CAPABILITIES);
        for retained in original
            .capabilities
            .iter()
            .filter(|cap| !optional_sync_slot(cap))
        {
            assert!(
                sync.capabilities.contains(retained),
                "lost {}",
                retained.name
            );
        }
        assert_eq!(native::with_folder_sync_upload(sync.clone()).unwrap(), sync);
        let original_selection = original.select().unwrap();
        assert_eq!(
            fr_wire::files::duplex_selected(&selected),
            fr_wire::files::duplex_selected(&original_selection)
        );
        assert_eq!(
            fr_wire::audio_control::downlink_selected(&selected),
            fr_wire::audio_control::downlink_selected(&original_selection)
        );
        assert_eq!(
            fr_wire::audio_up::uplink_selected(&selected),
            fr_wire::audio_up::uplink_selected(&original_selection)
        );
        assert_eq!(
            fr_wire::control::requestable(&selected.capabilities),
            fr_wire::control::requestable(&original_selection.capabilities)
        );
        for intersection in [
            original.intersect(&sync).unwrap(),
            sync.intersect(&original).unwrap(),
        ] {
            assert!(!sync_job_state::upload_selected(
                &intersection.select().unwrap()
            ));
            for retained in original
                .capabilities
                .iter()
                .filter(|cap| !optional_sync_slot(cap))
            {
                assert!(intersection.capabilities.contains(retained));
            }
        }
    }
    for unavailable in [
        native::observation_offer(),
        native::control_offer(),
        native::control_offer_with_download(false, false),
    ] {
        assert!(native::with_folder_sync_upload(unavailable).is_err());
    }
}

#[test]
fn both_explicit_job_directions_survive_complete_duplex_negotiation() {
    let original = native::control_offer_with_duplex(false, false);
    for upload_first in [false, true] {
        let prepared = if upload_first {
            with_folder_sync(native::with_folder_sync_upload(original.clone()).unwrap()).unwrap()
        } else {
            native::with_folder_sync_upload(with_folder_sync(original.clone()).unwrap()).unwrap()
        };
        let selected = prepared.select().unwrap();
        assert!(sync_job_state::selected(&selected));
        assert!(sync_job_state::upload_selected(&selected));
        assert!(fr_wire::files::duplex_selected(&selected));
        assert_eq!(prepared.limits, original.limits);
        let legacy = original.intersect(&prepared).unwrap().select().unwrap();
        assert!(fr_wire::files::duplex_selected(&legacy));
        assert!(!sync_job_state::selected(&legacy));
        assert!(!sync_job_state::upload_selected(&legacy));
    }
}

#[test]
fn native_flat_and_recursive_jobs_select_only_the_configured_direction_and_scope() {
    use sync_job_state::JobMode;
    for (mode, other) in [
        (JobMode::Flat, JobMode::Recursive),
        (JobMode::Recursive, JobMode::Flat),
    ] {
        for audio in [false, true] {
            for clipboard in [false, true] {
                let original = native::control_offer_with_download(clipboard, audio);
                let download = native::with_folder_sync_mode(original.clone(), mode).unwrap();
                let selection = download.select().unwrap();
                assert!(mode.download_selected(&selection));
                assert!(!other.download_selected(&selection));
                assert!(!mode.upload_selected(&selection));
                let peer = native::with_folder_sync_mode(original, other).unwrap();
                let mixed = download.intersect(&peer).unwrap().select().unwrap();
                assert!(!sync_job_state::selected(&mixed));

                let original = native::control_offer_with_duplex(clipboard, audio);
                let upload = native::with_folder_sync_upload_mode(original.clone(), mode).unwrap();
                let selection = upload.select().unwrap();
                assert!(mode.upload_selected(&selection));
                assert!(!other.upload_selected(&selection));
                assert!(!mode.download_selected(&selection));
                let peer = native::with_folder_sync_upload_mode(original, other).unwrap();
                let mixed = upload.intersect(&peer).unwrap().select().unwrap();
                assert!(!sync_job_state::upload_selected(&mixed));
            }
        }
        assert!(native::with_folder_sync_mode(native::observation_offer(), mode).is_err());
        assert!(native::with_folder_sync_upload_mode(native::observation_offer(), mode).is_err());
    }
}
