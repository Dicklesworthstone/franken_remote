use super::super::tests::snapshot;
use super::*;
use fr_files::sender::{Error, Progress};
fn text(row: &[u8; COLUMNS]) -> &str {
    std::str::from_utf8(&row[..row.iter().position(|b| *b == 0).unwrap()]).unwrap()
}
#[test]
fn fully_queued_still_requires_a_publication_receipt() {
    let mut s = snapshot();
    s.status.phase = SendPhase::Sending;
    s.stage = Some(Stage::AwaitingProof);
    s.current = Some(Progress {
        id: 17,
        queued_bytes: 4096,
        total_bytes: Some(4096),
    });
    let f = Frame::new(&s);
    assert_eq!(f.progress, 1000);
    assert!(text(&f.lines[0]).contains("publication receipt"));
    assert!(text(&f.lines[4]).contains("host 0, controller 0"));
    assert!(f.permits(Action::Cancel));
}
#[test]
fn empty_and_unknown_objects_do_not_invent_a_percentage() {
    let mut s = snapshot();
    for total_bytes in [None, Some(0)] {
        s.current = Some(Progress {
            id: 1,
            queued_bytes: 0,
            total_bytes,
        });
        assert_eq!(Frame::new(&s).progress, -1);
    }
    assert!(text(&Frame::new(&s).lines[2]).contains("0 / 0 bytes"));
}
#[test]
fn full_width_byte_counts_and_overreported_progress_are_bounded() {
    let mut s = snapshot();
    for (queued, total, expected) in [
        (u64::MAX, u64::MAX, 1000),
        (u64::MAX, 1, 1000),
        (u64::MAX / 2, u64::MAX, 499),
        (0, 1, 0),
    ] {
        s.current = Some(Progress {
            id: u64::MAX,
            queued_bytes: queued,
            total_bytes: Some(total),
        });
        let f = Frame::new(&s);
        assert_eq!(f.progress, expected);
        assert!(text(&f.lines[2]).contains(&queued.to_string()));
        assert!(text(&f.lines[2]).contains(&total.to_string()));
        for row in &f.lines {
            assert!(text(row).is_ascii());
        }
    }
}
#[test]
fn requested_and_observed_pacing_are_distinct_for_every_combination() {
    let mut s = snapshot();
    for requested in [false, true] {
        for observed in [None, Some(false), Some(true)] {
            s.pause_requested = requested;
            s.paused = observed;
            let f = Frame::new(&s);
            assert_eq!(f.permits(Action::Pause), !requested);
            assert_eq!(f.permits(Action::Resume), requested);
            assert!(f.permits(Action::Cancel));
            assert!(text(&f.lines[1]).starts_with(if requested {
                "Request: pause"
            } else {
                "Request: run"
            }));
            assert!(text(&f.lines[1]).ends_with(match observed {
                None => "unknown",
                Some(false) => "running",
                Some(true) => "paused",
            }));
        }
    }
}
#[test]
fn cancellation_and_all_terminal_phases_disable_every_command() {
    for phase in [
        SendPhase::WaitingForControl,
        SendPhase::Negotiating,
        SendPhase::WaitingForSelection,
        SendPhase::SelectionAccepted,
        SendPhase::Sending,
        SendPhase::Finished,
        SendPhase::Ended,
    ] {
        for cancelled in [false, true] {
            let mut s = snapshot();
            s.status.phase = phase;
            s.cancel_requested = cancelled;
            assert_eq!(
                Frame::new(&s).actions == 0,
                cancelled
                    || matches!(
                        phase,
                        SendPhase::SelectionAccepted | SendPhase::Finished | SendPhase::Ended
                    )
            );
        }
    }
}
#[test]
fn waiting_for_a_first_drop_exposes_only_cancel_and_no_transfer_evidence() {
    let mut sample = snapshot();
    sample.status.phase = SendPhase::WaitingForSelection;
    for stage in [None, Some(Stage::Idle)] {
        sample.stage = stage;
        let frame = Frame::new(&sample);
        assert_eq!(frame.progress, -1);
        assert!(frame.permits(Action::Cancel));
        assert!(!frame.permits(Action::Pause));
        assert!(!frame.permits(Action::Resume));
        assert!(text(&frame.lines[0]).starts_with(if stage.is_some() {
            "Ready to send files"
        } else {
            "Waiting for host file sharing"
        }));
        assert!(text(&frame.lines[2]).contains("Only the items you drop will be sent"));
        assert!(
            frame
                .lines
                .iter()
                .all(|row| !text(row).starts_with("Status exceeds"))
        );
    }
    sample.cancel_requested = true;
    let frame = Frame::new(&sample);
    assert_eq!(frame.actions, 0);
    assert!(text(&frame.lines[0]).starts_with("Stopping file drops"));
    sample.cancel_requested = false;
    sample.status.phase = SendPhase::SelectionAccepted;
    assert_eq!(Frame::new(&sample).actions, 0);
}
#[test]
fn every_sender_stage_is_rendered_without_changing_authority() {
    let mut s = snapshot();
    s.status.phase = SendPhase::Sending;
    for stage in [
        None,
        Some(Stage::Idle),
        Some(Stage::Queued),
        Some(Stage::Preparing),
        Some(Stage::AwaitingAcceptance),
        Some(Stage::Streaming),
        Some(Stage::AwaitingProof),
        Some(Stage::Finished),
        Some(Stage::Closed),
    ] {
        s.stage = stage;
        let f = Frame::new(&s);
        assert_ne!(text(&f.lines[0]), "");
        assert_eq!(f.progress, -1);
        assert!(f.permits(Action::Cancel));
    }
}
#[test]
fn publication_and_durability_uncertainty_preserve_the_recipient() {
    let mut o = Outcomes::default();
    o.observe(Outcome::HostPublished {
        bytes: u64::MAX,
        publication: Publication::Durable,
    });
    o.observe(Outcome::HostPublished {
        bytes: 0,
        publication: Publication::DurabilityUnknown,
    });
    o.observe(Outcome::ControllerPublished {
        bytes: 55,
        publication: Publication::DurabilityUnknown,
    });
    o.observe(Outcome::PublicationUnknown);
    o.observe(Outcome::InterruptedBeforePublication(Error::Cancelled));
    assert_eq!(
        o,
        Outcomes {
            host: 2,
            controller: 1,
            durability_unknown: 2,
            unknown: 1,
            interrupted: 1,
            refused: 0
        }
    );
}
#[test]
fn cleanup_does_not_rewrite_publication_evidence() {
    let mut s = snapshot();
    for cleanup in [None, Some(Ok(())), Some(Err(Error::Cancelled))] {
        s.cleanup = cleanup;
        let f = Frame::new(&s);
        assert!(text(&f.lines[4]).contains("host 0, controller 0"));
        assert!(text(&f.lines[6]).starts_with("Local source cleanup:"));
    }
}
#[test]
fn overlong_or_non_ascii_text_fails_closed_instead_of_truncating_numbers() {
    for value in ["9".repeat(COLUMNS), "\u{2603}".into()] {
        assert!(text(&line(format_args!("{value}"))).starts_with("Status exceeds"));
    }
    assert_eq!(std::mem::size_of::<Frame>(), ROWS * COLUMNS + 8);
}
