//! Recursive v2 is exact bilateral consent on each original file direction.
use fr_core::limits::ProtocolLimits;
use fr_wire::{
    attachment, files,
    negotiation::{self, Capability, Offer, Role, Selection},
    sync_job_state::{self, RECURSIVE_VERSION},
};

fn plain(upload: bool) -> Offer {
    let mut offer = Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: Role::RequestControl,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: Vec::new(),
    };
    for (name, version) in [
        (attachment::CAPABILITY, attachment::VERSION),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
        (files::CAPABILITY, files::VERSION),
        if upload {
            (
                files::CHANNEL_SCOPE_CAPABILITY,
                files::CHANNEL_SCOPE_VERSION,
            )
        } else {
            (files::DOWNLOAD_CAPABILITY, files::DOWNLOAD_VERSION)
        },
    ] {
        add(&mut offer, name, version);
    }
    offer
}

fn add(offer: &mut Offer, name: &str, version: u16) {
    offer.capabilities.push(Capability {
        name: name.into(),
        version,
        required: false,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
}

fn marker(upload: bool) -> &'static str {
    if upload {
        sync_job_state::UPLOAD_CAPABILITY
    } else {
        sync_job_state::CAPABILITY
    }
}

fn prepare(offer: Offer, upload: bool, recursive: bool) -> Result<Offer, negotiation::Error> {
    match (upload, recursive) {
        (false, false) => sync_job_state::prepare_offer(offer),
        (false, true) => sync_job_state::prepare_recursive_offer(offer),
        (true, false) => sync_job_state::prepare_upload_offer(offer),
        (true, true) => sync_job_state::prepare_recursive_upload_offer(offer),
    }
}

fn selected(selection: &Selection, upload: bool) -> bool {
    if upload {
        sync_job_state::upload_selected(selection)
    } else {
        sync_job_state::selected(selection)
    }
}

fn recursive_selected(selection: &Selection, upload: bool) -> bool {
    if upload {
        sync_job_state::recursive_upload_selected(selection)
    } else {
        sync_job_state::recursive_selected(selection)
    }
}

#[test]
fn explicit_recursive_upgrade_is_idempotent_and_uses_the_existing_capability_slot() {
    for upload in [false, true] {
        let plain = plain(upload);
        let flat = prepare(plain.clone(), upload, false).unwrap();
        assert!(selected(&flat.select().unwrap(), upload));
        assert!(!recursive_selected(&flat.select().unwrap(), upload));
        let recursive = prepare(plain, upload, true).unwrap();
        assert_eq!(recursive.capabilities.len(), flat.capabilities.len());
        assert_eq!(prepare(flat, upload, true).unwrap(), recursive);
        assert_eq!(prepare(recursive.clone(), upload, true).unwrap(), recursive);
        assert!(prepare(recursive.clone(), upload, false).is_err());
        let selection = recursive.select().unwrap();
        assert!(selected(&selection, upload));
        assert!(recursive_selected(&selection, upload));
        assert!(!selected(&selection, !upload));
        assert!(!recursive_selected(&selection, !upload));
    }
}

#[test]
fn v1_v2_and_unknown_peers_never_silently_downgrade_a_recursive_job() {
    for upload in [false, true] {
        for host_version in 1..=3 {
            for peer_version in 1..=3 {
                let mut host = plain(upload);
                let mut peer = host.clone();
                add(&mut host, marker(upload), host_version);
                add(&mut peer, marker(upload), peer_version);
                let intersection = host.intersect(&peer).unwrap();
                host.check_host(&intersection).unwrap();
                peer.check_host(&intersection).unwrap();
                let selection = intersection.select().unwrap();
                for mode in [
                    sync_job_state::JobMode::Flat,
                    sync_job_state::JobMode::Recursive,
                ] {
                    let exact = if upload {
                        mode.upload_selected(&selection)
                    } else {
                        mode.download_selected(&selection)
                    };
                    let expected_version = match mode {
                        sync_job_state::JobMode::Flat => 1,
                        sync_job_state::JobMode::Recursive => RECURSIVE_VERSION,
                    };
                    assert_eq!(
                        exact,
                        host_version == expected_version && peer_version == expected_version
                    );
                }
                assert_eq!(
                    selected(&selection, upload),
                    host_version == peer_version && host_version <= RECURSIVE_VERSION
                );
                assert_eq!(
                    recursive_selected(&selection, upload),
                    host_version == RECURSIVE_VERSION && peer_version == RECURSIVE_VERSION
                );
            }
        }
    }
}

#[test]
fn recursive_markers_cannot_replace_any_original_attachment_atp_or_direction_gate() {
    for upload in [false, true] {
        let plain = plain(upload);
        let complete = prepare(plain.clone(), upload, true).unwrap();
        for prerequisite in &plain.capabilities {
            for unknown in [false, true] {
                let mut malformed = complete.clone();
                let mut unprepared = plain.clone();
                for offer in [&mut malformed, &mut unprepared] {
                    if unknown {
                        offer
                            .capabilities
                            .iter_mut()
                            .find(|cap| cap.name == prerequisite.name)
                            .unwrap()
                            .version += 1;
                    } else {
                        offer
                            .capabilities
                            .retain(|cap| cap.name != prerequisite.name);
                    }
                }
                assert!(prepare(unprepared, upload, true).is_err());
                // Even a syntactically valid, forged selection with v2 is
                // insufficient without every original positive boundary.
                let selection = malformed.select().unwrap();
                assert!(!selected(&selection, upload), "{}", prerequisite.name);
                assert!(!recursive_selected(&selection, upload));
                let selection = complete.intersect(&malformed).unwrap().select().unwrap();
                assert!(!selected(&selection, upload));
                assert!(!recursive_selected(&selection, upload));
            }
        }
        let selection = complete.select().unwrap();
        let mut observer = selection.clone();
        observer.role = Role::Observe;
        assert!(!selected(&observer, upload));
        assert!(!recursive_selected(&observer, upload));
        let mut duplicate = selection.clone();
        duplicate
            .capabilities
            .push(selection.capabilities[0].clone());
        assert!(!recursive_selected(&duplicate, upload));
        let mut unordered = selection;
        unordered.capabilities.reverse();
        assert!(!recursive_selected(&unordered, upload));
    }
}

#[test]
fn recursive_directions_are_independent_and_upload_still_requires_complete_duplex() {
    let mut both = plain(true);
    add(
        &mut both,
        files::DOWNLOAD_CAPABILITY,
        files::DOWNLOAD_VERSION,
    );
    let mut forged = both.clone();
    add(
        &mut forged,
        sync_job_state::UPLOAD_CAPABILITY,
        RECURSIVE_VERSION,
    );
    assert!(sync_job_state::prepare_recursive_upload_offer(both.clone()).is_err());
    assert!(!sync_job_state::upload_selected(&forged.select().unwrap()));
    assert!(!sync_job_state::recursive_upload_selected(
        &forged.select().unwrap()
    ));
    add(&mut both, files::DUPLEX_CAPABILITY, files::DUPLEX_VERSION);
    for recursive_upload in [false, true] {
        let mixed = prepare(
            prepare(both.clone(), false, !recursive_upload).unwrap(),
            true,
            recursive_upload,
        )
        .unwrap();
        let selection = mixed.select().unwrap();
        assert!(files::duplex_selected(&selection));
        assert!(sync_job_state::selected(&selection));
        assert!(sync_job_state::upload_selected(&selection));
        assert_eq!(
            sync_job_state::recursive_selected(&selection),
            !recursive_upload
        );
        assert_eq!(
            sync_job_state::recursive_upload_selected(&selection),
            recursive_upload
        );
        for missing in [
            sync_job_state::CAPABILITY,
            sync_job_state::UPLOAD_CAPABILITY,
        ] {
            let mut peer = mixed.clone();
            peer.capabilities.retain(|cap| cap.name != missing);
            let selection = mixed.intersect(&peer).unwrap().select().unwrap();
            assert_eq!(
                sync_job_state::selected(&selection),
                missing != sync_job_state::CAPABILITY
            );
            assert_eq!(
                sync_job_state::upload_selected(&selection),
                missing != sync_job_state::UPLOAD_CAPABILITY
            );
        }
    }
}

#[test]
fn recursive_upgrade_at_capacity_preserves_every_other_capability_and_refuses_required_markers() {
    for upload in [false, true] {
        let mut full = prepare(plain(upload), upload, false).unwrap();
        while full.capabilities.len() < negotiation::MAX_CAPABILITIES {
            let name = format!("zz-required-{}", full.capabilities.len());
            add(&mut full, &name, 1);
            full.capabilities
                .iter_mut()
                .find(|cap| cap.name == name)
                .unwrap()
                .required = true;
        }
        let mut expected = full.clone();
        expected
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == marker(upload))
            .unwrap()
            .version = RECURSIVE_VERSION;
        assert_eq!(prepare(full.clone(), upload, true).unwrap(), expected);
        for (required, version) in [(true, 1), (true, RECURSIVE_VERSION), (false, 3)] {
            let mut incompatible = full.clone();
            let cap = incompatible
                .capabilities
                .iter_mut()
                .find(|cap| cap.name == marker(upload))
                .unwrap();
            cap.required = required;
            cap.version = version;
            assert!(prepare(incompatible, upload, true).is_err());
        }
    }
}
