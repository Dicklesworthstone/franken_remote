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
                let sync = with_folder_sync(original.clone()).unwrap();
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
