//! Completion projection fixtures use the production typed-outcome projection.
//! They prove global indexing/report retention, not authenticated publication.
use super::*;

fn json(summary: &Summary) -> serde_json::Value {
    serde_json::from_str(&format!("{{\"schema_version\":1{}}}", summary.json())).unwrap()
}

fn control_with_original_bulk_framing() -> fr_wire::negotiation::Offer {
    let mut offer = fr_client::native::control_offer();
    // Retiring file consent preserves the original optional framing boundary;
    // a new bare control constructor has no reason to request bulk records.
    offer.capabilities.push(fr_wire::negotiation::Capability {
        name: fr_wire::attachment::BULK_RECORD_CAPABILITY.into(),
        version: fr_wire::attachment::BULK_RECORD_VERSION,
        required: false,
    });
    offer.capabilities.sort_by(|a, b| a.name.cmp(&b.name));
    offer
}

#[test]
fn spent_command_only_consent_cannot_reopen_on_a_precontrol_retry() {
    let mut local = select(&[], false, true).unwrap().unwrap();
    assert!(local.can_configure_upload());
    local.command_connection(&"a".repeat(32));
    let configuration = || {
        fr_native::desktop::Configuration::new(
            std::path::Path::new("/usr/bin/false"),
            ":0",
            None,
            12,
        )
        .unwrap()
    };
    let mut first = Desktop::new(configuration());
    // Setup refusal is still a spent command consent; no live viewer or lease
    // is fabricated by this pre-control lifecycle fixture.
    local.configure(&mut first);
    assert!(!local.can_configure_upload());
    local.command_connection(&"b".repeat(32));
    assert_eq!(
        local.commands.as_ref().unwrap().connection.as_deref(),
        Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    let mut next = Desktop::new(configuration());
    local.configure(&mut next);
    assert!(!local.can_configure_upload());
    assert_eq!(local.summary().requested, 0);
    let mut retry = fr_client::native::control_offer_with_files();
    retain_offer(&mut retry, local.can_configure_upload(), false);
    assert_eq!(retry, control_with_original_bulk_framing());
}

#[test]
fn spent_command_upload_keeps_independent_download_and_drop_choices() {
    let mut command_only = select(&[], false, true).unwrap().unwrap();
    command_only.refuse_commands();
    let mut retry = fr_client::native::control_offer_with_duplex(true, true);
    retain_offer(&mut retry, command_only.can_configure_upload(), true);
    assert!(
        retry
            .capabilities
            .iter()
            .any(|cap| cap.name == fr_wire::files::DOWNLOAD_CAPABILITY)
    );
    assert!(
        retry
            .capabilities
            .iter()
            .any(|cap| cap.name == fr_wire::attachment::FILES_CAPABILITY)
    );
    assert!(
        retry
            .capabilities
            .iter()
            .any(|cap| cap.name == fr_wire::files::CAPABILITY)
    );
    assert!(!retry.capabilities.iter().any(|cap| matches!(
        cap.name.as_str(),
        fr_wire::files::CHANNEL_SCOPE_CAPABILITY | fr_wire::files::DUPLEX_CAPABILITY
    )));
    let mut drop = select(&[], true, true).unwrap().unwrap();
    drop.refuse_commands();
    assert!(drop.can_configure_upload());
    let original = fr_client::native::control_offer_with_duplex(false, false);
    let mut both = original.clone();
    retain_offer(&mut both, drop.can_configure_upload(), true);
    assert_eq!(both, original);
    retain_offer(&mut both, false, false);
    assert_eq!(both, control_with_original_bulk_framing());
}

#[test]
fn final_command_requests_map_to_their_exact_combined_outcomes() {
    let mut complete = reported(
        &[Some(5)],
        [FileSendOutcome::PublicationUnknown].into_iter(),
    );
    complete.append_dropped(&DroppedBatchStatus {
        sizes: vec![Some(7)],
        status: Err("file_selection_stopped"),
    });
    complete.commands = Some(Ok(()));
    for (sequence, sizes) in [(1, vec![Some(11), None]), (2, vec![Some(13)])] {
        complete.append_command(&local::BatchStatus {
            sequence,
            sizes,
            status: SendStatus {
                phase: SendPhase::WaitingForControl,
                absence: None,
                report: None,
            },
        });
    }
    let output = json(&complete);
    assert_eq!(
        output["files_commands"]["requests"],
        serde_json::json!([
            { "request": 1, "first_index": 2, "selected": 2 },
            { "request": 2, "first_index": 4, "selected": 1 },
        ])
    );
    assert_eq!(output["files_refused"][0]["reason"], "publication_unknown");
    assert_eq!(
        output["files_refused"][1]["reason"],
        "file_selection_stopped"
    );
    assert_eq!(output["files_refused"][2]["reason"], "files_queued");
    assert_eq!(output["files_refused"][4]["bytes"], 13);
    assert!(complete.text().contains("File request 1: #3 through #4"));
    assert!(complete.text().contains("File request 2: #5 through #5"));
}

#[test]
fn later_refused_preparing_and_queued_selections_preserve_initial_publication() {
    let mut complete = reported(
        &[Some(19), Some(0)],
        [
            FileSendOutcome::HostPublished {
                bytes: 19,
                publication: Publication::Durable,
            },
            FileSendOutcome::HostPublished {
                bytes: 0,
                publication: Publication::DurabilityUnknown,
            },
        ]
        .into_iter(),
    );
    for batch in [
        DroppedBatchStatus {
            sizes: vec![None, Some(7)],
            status: Err("send_symlink"),
        },
        DroppedBatchStatus {
            sizes: vec![None],
            status: Err("file_selection_preparing"),
        },
        DroppedBatchStatus {
            sizes: vec![Some(3)],
            status: Ok(SendStatus {
                phase: SendPhase::WaitingForControl,
                absence: None,
                report: None,
            }),
        },
    ] {
        complete.append_dropped(&batch);
    }
    assert_eq!(complete.requested, 6);
    assert_eq!(complete.sent, [(0, 19, true), (1, 0, false)]);
    assert_eq!(
        complete.refused,
        [
            (2, None, "send_symlink"),
            (3, Some(7), "send_symlink"),
            (4, None, "file_selection_preparing"),
            (5, Some(3), "files_queued"),
        ]
    );
    assert_eq!(complete.absence, None);
    assert_eq!(
        complete.text(),
        " Files: 2 of 6 published by the host; #3 send_symlink; #4 send_symlink; #5 file_selection_preparing; #6 files_queued."
    );
    let output = json(&complete);
    assert!(output["files_absence"].is_null());
    assert_eq!(output["files_requested"], 6);
    assert_eq!(output["files_sent"][0]["bytes"], 19);
    assert_eq!(output["files_sent"][1]["durable"], false);
    assert_eq!(output["files_refused"][0]["index"], 2);
    assert!(output["files_refused"][0]["bytes"].is_null());
}

#[test]
fn every_batch_keeps_global_indexes_and_actual_outcome_order() {
    let mut complete = reported(
        &[Some(5), Some(7)],
        [
            FileSendOutcome::HostPublished {
                bytes: 5,
                publication: Publication::Durable,
            },
            FileSendOutcome::HostRefused(Reason::Conflict),
        ]
        .into_iter(),
    );
    complete.append(reported(
        &[None, Some(11), Some(13)],
        [
            FileSendOutcome::PublicationUnknown,
            FileSendOutcome::HostPublished {
                bytes: 11,
                publication: Publication::Durable,
            },
        ]
        .into_iter(),
    ));
    complete.append_dropped(&DroppedBatchStatus {
        sizes: vec![Some(17)],
        status: Err("file_selection_stopped"),
    });
    assert_eq!(complete.requested, 6);
    assert_eq!(complete.sent, [(0, 5, true), (3, 11, true)]);
    assert_eq!(
        complete.refused,
        [
            (1, Some(7), "host_conflict"),
            (2, None, "publication_unknown"),
            (4, Some(13), "not_started"),
            (5, Some(17), "file_selection_stopped"),
        ]
    );
    let mut indexes: Vec<_> = complete
        .sent
        .iter()
        .map(|(index, _, _)| *index)
        .chain(complete.refused.iter().map(|(index, _, _)| *index))
        .collect();
    indexes.sort_unstable();
    assert_eq!(indexes, (0..6).collect::<Vec<_>>());
    let output = json(&complete);
    assert_eq!(output["files_sent"][1]["index"], 3);
    assert_eq!(output["files_refused"][1]["reason"], "publication_unknown");
    assert!(output["files_absence"].is_null());
    assert!(!complete.text().contains("nothing sent"));
}

#[test]
fn aggregate_absence_cannot_override_any_batch_report() {
    let mut complete = summary(&[Some(2)], Err("files_not_started"));
    complete.append_dropped(&DroppedBatchStatus {
        sizes: vec![None],
        status: Err("file_selection_preparing"),
    });
    assert_eq!(complete.absence, Some("files_not_started"));
    assert_eq!(complete.requested, 2);
    complete.append(reported(
        &[Some(3)],
        [FileSendOutcome::HostRefused(Reason::Resource)].into_iter(),
    ));
    complete.append_dropped(&DroppedBatchStatus {
        sizes: vec![Some(4)],
        status: Ok(SendStatus {
            phase: SendPhase::Ended,
            absence: Some(Absence::Unavailable),
            report: None,
        }),
    });
    assert_eq!(complete.absence, None);
    assert_eq!(complete.sent, [] as [(usize, u64, bool); 0]);
    assert_eq!(complete.requested, 4);
    assert_eq!(
        complete.refused,
        [
            (0, Some(2), "not_sent"),
            (1, None, "file_selection_preparing"),
            (2, Some(3), "host_limit"),
            (3, Some(4), "files_unavailable"),
        ]
    );
    assert_eq!(
        complete.text(),
        " Files: 0 of 4 published by the host; #1 not_sent; #2 file_selection_preparing; #3 host_limit; #4 files_unavailable."
    );
    assert!(json(&complete)["files_absence"].is_null());
}
