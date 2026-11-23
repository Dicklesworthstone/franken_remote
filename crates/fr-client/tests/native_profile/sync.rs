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
