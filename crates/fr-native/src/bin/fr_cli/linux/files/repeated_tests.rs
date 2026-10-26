//! Completion projection fixtures use the production typed-outcome projection.
//! They prove global indexing/report retention, not authenticated publication.
use super::*;

fn json(summary: &Summary) -> serde_json::Value {
    serde_json::from_str(&format!("{{\"schema_version\":1{}}}", summary.json())).unwrap()
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
    assert!(complete.sent.is_empty());
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
