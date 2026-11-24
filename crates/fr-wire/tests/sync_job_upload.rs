//! Independent upload-job selection and the original host/controller consent roles.
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_wire::{
    WireError, attachment,
    files::{self, Context, Direction, Lane, Limits, Reason, Role},
    negotiation::{self, Capability, Offer, Role as SessionRole},
    sync_job_state::{self, ConflictPolicy, SyncDirection, SyncJobState, SyncPhase},
};

fn plain_upload() -> Offer {
    let mut offer = Offer {
        versions: vec![0],
        profile: 1,
        profile_version: 0,
        role: SessionRole::RequestControl,
        limits: ProtocolLimits::ABSOLUTE,
        capabilities: Vec::new(),
    };
    for (name, version) in [
        (attachment::CAPABILITY, attachment::VERSION),
        (attachment::INPUT_CAPABILITY, attachment::INPUT_VERSION),
        (attachment::FILES_CAPABILITY, attachment::FILES_VERSION),
        (files::CAPABILITY, files::VERSION),
        (
            files::CHANNEL_SCOPE_CAPABILITY,
            files::CHANNEL_SCOPE_VERSION,
        ),
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

#[test]
fn upload_jobs_require_bilateral_exact_control_and_original_upload_boundaries() {
    let complete = sync_job_state::prepare_upload_offer(plain_upload()).unwrap();
    let selected = complete.select().unwrap();
    assert!(sync_job_state::upload_selected(&selected));
    assert!(!sync_job_state::selected(&selected));
    assert!(!files::duplex_selected(&selected));
    for capability in &complete.capabilities {
        for unknown_version in [false, true] {
            let mut peer = complete.clone();
            if unknown_version {
                peer.capabilities
                    .iter_mut()
                    .find(|cap| cap.name == capability.name)
                    .unwrap()
                    .version += 1;
            } else {
                peer.capabilities.retain(|cap| cap.name != capability.name);
            }
            let selected = complete.intersect(&peer).unwrap().select().unwrap();
            assert!(
                !sync_job_state::upload_selected(&selected),
                "missing exact {}",
                capability.name
            );
        }
    }
    let mut observer = selected.clone();
    observer.role = SessionRole::Observe;
    assert!(!sync_job_state::upload_selected(&observer));
    let mut duplicated = selected.clone();
    duplicated
        .capabilities
        .push(selected.capabilities[0].clone());
    assert!(!sync_job_state::upload_selected(&duplicated));
    let mut unordered = selected;
    unordered.capabilities.reverse();
    assert!(!sync_job_state::upload_selected(&unordered));
}

#[test]
fn each_sync_direction_stays_independent_and_two_file_directions_require_duplex() {
    let plain = plain_upload();
    let upload = sync_job_state::prepare_upload_offer(plain.clone()).unwrap();
    let legacy = plain.intersect(&upload).unwrap().select().unwrap();
    assert!(!sync_job_state::upload_selected(&legacy));
    assert!(!sync_job_state::selected(&legacy));
    let mut download = plain.clone();
    download
        .capabilities
        .retain(|cap| cap.name != files::CHANNEL_SCOPE_CAPABILITY);
    add(
        &mut download,
        files::DOWNLOAD_CAPABILITY,
        files::DOWNLOAD_VERSION,
    );
    let download = sync_job_state::prepare_offer(download).unwrap();
    assert!(sync_job_state::selected(&download.select().unwrap()));
    assert!(!sync_job_state::upload_selected(
        &download.select().unwrap()
    ));
    assert!(sync_job_state::prepare_upload_offer(download).is_err());

    let mut ambiguous = upload;
    add(
        &mut ambiguous,
        files::DOWNLOAD_CAPABILITY,
        files::DOWNLOAD_VERSION,
    );
    assert!(!sync_job_state::upload_selected(
        &ambiguous.select().unwrap()
    ));
    assert!(sync_job_state::prepare_upload_offer(ambiguous.clone()).is_err());
    add(
        &mut ambiguous,
        files::DUPLEX_CAPABILITY,
        files::DUPLEX_VERSION,
    );
    let both = sync_job_state::prepare_offer(ambiguous).unwrap();
    let selected = both.select().unwrap();
    assert!(files::duplex_selected(&selected));
    assert!(sync_job_state::selected(&selected));
    assert!(sync_job_state::upload_selected(&selected));
    assert_eq!(
        sync_job_state::prepare_upload_offer(both.clone()).unwrap(),
        both
    );
    for missing in [
        sync_job_state::CAPABILITY,
        sync_job_state::UPLOAD_CAPABILITY,
    ] {
        let mut peer = both.clone();
        peer.capabilities.retain(|cap| cap.name != missing);
        let selected = both.intersect(&peer).unwrap().select().unwrap();
        assert!(files::duplex_selected(&selected));
        assert_eq!(
            sync_job_state::selected(&selected),
            missing != sync_job_state::CAPABILITY
        );
        assert_eq!(
            sync_job_state::upload_selected(&selected),
            missing != sync_job_state::UPLOAD_CAPABILITY
        );
    }
}

#[test]
fn upload_helper_is_idempotent_and_requires_a_complete_original_drop_profile() {
    let plain = plain_upload();
    let prepared = sync_job_state::prepare_upload_offer(plain.clone()).unwrap();
    assert_eq!(
        sync_job_state::prepare_upload_offer(prepared.clone()).unwrap(),
        prepared
    );
    for cap in &plain.capabilities {
        let mut missing = plain.clone();
        missing
            .capabilities
            .retain(|candidate| candidate.name != cap.name);
        assert!(sync_job_state::prepare_upload_offer(missing).is_err());
    }
    for (required, version) in [(true, 1), (false, 2)] {
        let mut incompatible = prepared.clone();
        let cap = incompatible
            .capabilities
            .iter_mut()
            .find(|cap| cap.name == sync_job_state::UPLOAD_CAPABILITY)
            .unwrap();
        cap.required = required;
        cap.version = version;
        assert!(sync_job_state::prepare_upload_offer(incompatible).is_err());
    }
}

#[test]
fn full_upload_profiles_preserve_features_required_caps_unknown_versions_and_limits() {
    let mut full = plain_upload();
    while full.capabilities.len() < negotiation::MAX_CAPABILITIES {
        full.capabilities.push(Capability {
            name: format!("zz-required-{}", full.capabilities.len()),
            version: 1,
            required: true,
        });
    }
    full.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    assert!(sync_job_state::prepare_upload_offer(full.clone()).is_err());
    for (name, version) in [
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
    ] {
        let mut candidate = full.clone();
        *candidate.capabilities.last_mut().unwrap() = Capability {
            name: name.into(),
            version,
            required: false,
        };
        candidate.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
        let prepared = sync_job_state::prepare_upload_offer(candidate.clone()).unwrap();
        assert_eq!(prepared.limits, candidate.limits);
        assert_eq!(prepared.capabilities.len(), negotiation::MAX_CAPABILITIES);
        for retained in candidate.capabilities.iter().filter(|cap| cap.name != name) {
            assert!(prepared.capabilities.contains(retained));
        }
        assert!(!prepared.capabilities.iter().any(|cap| cap.name == name));
        for (required, altered_version) in [(true, version), (false, version + 1)] {
            let mut protected = candidate.clone();
            let cap = protected
                .capabilities
                .iter_mut()
                .find(|cap| cap.name == name)
                .unwrap();
            cap.required = required;
            cap.version = altered_version;
            assert!(sync_job_state::prepare_upload_offer(protected).is_err());
        }
    }
}

#[test]
fn upload_job_still_requires_host_advertisement_then_controller_acceptance() {
    let context = Context {
        session: RemoteSessionId::from_raw(11),
        lease: InputLeaseId::from_raw(12),
        handle: 13,
        channel: 14,
        sender: Role::Host,
        direction: Direction::ToHost,
        lane: Lane::Files,
    };
    let limits = Limits::new(&ProtocolLimits::ABSOLUTE, 4096).unwrap();
    let advertised = SyncJobState {
        job_id: 41,
        sync_direction: SyncDirection::ControllerToHost,
        conflict_policy: ConflictPolicy::KeepBoth,
        phase: SyncPhase::Advertised,
        reason: Reason::None,
        entries_total: 0,
        entries_completed: 0,
        conflicts_kept: 0,
        bytes_total: 0,
        bytes_completed: 0,
        manifest_revision: 0,
    };
    let accepted = SyncJobState {
        phase: SyncPhase::Accepted,
        ..advertised
    };
    let mut bytes = [0; sync_job_state::RECORD_BYTES];
    for (state, sender) in [(advertised, Role::Host), (accepted, Role::Controller)] {
        let context = Context { sender, ..context };
        let length = sync_job_state::encode(&state, context, limits, &mut bytes).unwrap();
        assert_eq!(length, sync_job_state::RECORD_BYTES);
        assert_eq!(sync_job_state::decode(&bytes, context, limits), Ok(state));
    }
    for (state, sender) in [
        (advertised, Role::Controller),
        (accepted, Role::Host),
        (advertised, Role::Observer),
        (accepted, Role::Observer),
    ] {
        assert_eq!(
            sync_job_state::encode(&state, Context { sender, ..context }, limits, &mut bytes),
            Err(WireError::WrongRole)
        );
    }
}
