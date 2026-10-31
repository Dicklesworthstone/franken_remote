use super::{CreditUpdate, ReceiverCreditWindow, ReleasedCredit, WssError};

fn release(credit_epoch: u64, released_bytes: u64, released_frames: u64) -> ReleasedCredit {
    ReleasedCredit {
        credit_epoch,
        released_bytes,
        released_frames,
    }
}

#[test]
fn whole_allocation_charges_obey_both_negotiated_allowances() {
    let mut credit = ReceiverCreditWindow::new(7, 10_000, 3).unwrap();
    assert_eq!(credit.credit_epoch(), 7);
    for bytes in [4_000, 4_000, 1_000] {
        assert!(credit.can_send(bytes));
        credit.try_reserve(bytes).unwrap();
    }
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (9_000, 3)
    );
    assert!(!credit.can_send(500));
    assert_eq!(
        credit.try_reserve(500),
        Err(WssError::CreditStarvation(9_000, 10_000, 3, 3))
    );

    assert_eq!(
        credit.return_credit(release(7, 4_000, 1)),
        Ok(CreditUpdate::Applied)
    );
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (5_000, 2)
    );
    assert!(!credit.can_send(5_001));
    assert_eq!(
        credit.try_reserve(5_001),
        Err(WssError::CreditStarvation(5_000, 10_000, 2, 3))
    );
    credit.try_reserve(5_000).unwrap();
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (10_000, 3)
    );
}

#[test]
fn replayed_release_cannot_pay_for_a_new_frame() {
    let mut credit = ReceiverCreditWindow::new(7, 8, 2).unwrap();
    credit.try_reserve(4).unwrap();
    credit.try_reserve(4).unwrap();
    let first = release(7, 4, 1);
    assert_eq!(credit.return_credit(first), Ok(CreditUpdate::Applied));
    credit.try_reserve(4).unwrap();
    for stale in [first, release(7, 0, 0), first] {
        assert_eq!(credit.return_credit(stale), Ok(CreditUpdate::Ignored));
        assert_eq!(
            (credit.outstanding_bytes(), credit.outstanding_frames()),
            (8, 2)
        );
        assert!(!credit.can_send(1));
    }
}

#[test]
fn release_reports_are_cumulative_not_bounded_by_the_window_size() {
    let mut credit = ReceiverCreditWindow::new(19, 7, 1).unwrap();
    for frame in 1..=1_024 {
        credit.try_reserve(7).unwrap();
        assert_eq!(
            (credit.outstanding_bytes(), credit.outstanding_frames()),
            (7, 1)
        );
        assert_eq!(
            credit.return_credit(release(19, frame * 7, frame)),
            Ok(CreditUpdate::Applied)
        );
        assert_eq!(
            (credit.outstanding_bytes(), credit.outstanding_frames()),
            (0, 0)
        );
    }
    assert_eq!(
        credit.return_credit(release(19, 7, 1)),
        Ok(CreditUpdate::Ignored)
    );
    credit.try_reserve(7).unwrap();
    assert!(!credit.can_send(1));
}

#[test]
fn each_resource_dimension_waits_for_its_own_actual_release() {
    let mut bytes = ReceiverCreditWindow::new(7, 8, 2).unwrap();
    bytes.try_reserve(8).unwrap();
    bytes.return_credit(release(7, 0, 1)).unwrap();
    assert_eq!(
        (bytes.outstanding_bytes(), bytes.outstanding_frames()),
        (8, 0)
    );
    assert!(!bytes.can_send(1));
    bytes.return_credit(release(7, 8, 1)).unwrap();
    assert!(bytes.can_send(8));

    let mut frames = ReceiverCreditWindow::new(7, 8, 1).unwrap();
    frames.try_reserve(4).unwrap();
    frames.return_credit(release(7, 4, 0)).unwrap();
    assert_eq!(
        (frames.outstanding_bytes(), frames.outstanding_frames()),
        (0, 1)
    );
    assert!(!frames.can_send(1));
    frames.return_credit(release(7, 4, 1)).unwrap();
    assert!(frames.can_send(8));
}

#[test]
fn older_dimensions_never_undo_a_newer_release() {
    let mut credit = ReceiverCreditWindow::new(7, 12, 3).unwrap();
    for _ in 0..3 {
        credit.try_reserve(4).unwrap();
    }
    credit.return_credit(release(7, 8, 1)).unwrap();
    assert_eq!(
        credit.return_credit(release(7, 4, 2)),
        Ok(CreditUpdate::Applied)
    );
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (4, 1)
    );
    assert_eq!(
        credit.return_credit(release(7, 8, 1)),
        Ok(CreditUpdate::Ignored)
    );
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (4, 1)
    );
}

#[test]
fn either_counter_overclaim_refuses_the_entire_report() {
    let mut credit = ReceiverCreditWindow::new(7, 8, 2).unwrap();
    credit.try_reserve(4).unwrap();
    credit.try_reserve(4).unwrap();
    credit.return_credit(release(7, 4, 1)).unwrap();
    for invalid in [
        release(7, 9, 2),
        release(7, 8, 3),
        release(7, 0, 3),
        release(7, 9, 0),
    ] {
        assert_eq!(
            credit.return_credit(invalid),
            Err(WssError::CreditReleaseExceedsConsumption)
        );
        assert_eq!(
            (credit.outstanding_bytes(), credit.outstanding_frames()),
            (4, 1)
        );
    }
    assert_eq!(
        credit.return_credit(release(7, 8, 2)),
        Ok(CreditUpdate::Applied)
    );
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (0, 0)
    );
}

#[test]
fn a_replaced_channel_cannot_reuse_its_predecessors_credit() {
    let mut previous = ReceiverCreditWindow::new(7, 8, 2).unwrap();
    previous.try_reserve(8).unwrap();
    let old = release(7, 8, 1);
    previous.return_credit(old).unwrap();

    let mut current = ReceiverCreditWindow::new(8, 8, 2).unwrap();
    current.try_reserve(8).unwrap();
    for foreign in [old, release(0, 0, 0), release(9, 8, 1)] {
        assert_eq!(
            current.return_credit(foreign),
            Err(WssError::CreditEpochMismatch {
                expected: 8,
                received: foreign.credit_epoch,
            })
        );
        assert_eq!(
            (current.outstanding_bytes(), current.outstanding_frames()),
            (8, 1)
        );
    }
    current.return_credit(release(8, 8, 1)).unwrap();
    assert_eq!(
        (current.outstanding_bytes(), current.outstanding_frames()),
        (0, 0)
    );
}

#[test]
fn invalid_windows_and_zero_cost_frames_are_refused() {
    for (epoch, bytes, frames) in [(0, 8, 2), (7, 0, 2), (7, 8, 0)] {
        assert!(matches!(
            ReceiverCreditWindow::new(epoch, bytes, frames),
            Err(WssError::InvalidCreditWindow)
        ));
    }
    let mut credit = ReceiverCreditWindow::new(7, 8, 2).unwrap();
    assert!(!credit.can_send(0));
    assert_eq!(credit.try_reserve(0), Err(WssError::InvalidCreditCharge));
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (0, 0)
    );
}

#[test]
fn exhausted_byte_counter_never_wraps_or_restarts_after_release() {
    let mut credit = ReceiverCreditWindow::new(7, u64::MAX, 2).unwrap();
    credit.try_reserve(u64::MAX).unwrap();
    assert!(!credit.can_send(1));
    assert_eq!(credit.try_reserve(1), Err(WssError::CreditCounterExhausted));
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (u64::MAX, 1)
    );
    credit.return_credit(release(7, u64::MAX, 1)).unwrap();
    assert!(!credit.can_send(1));
    assert_eq!(credit.try_reserve(1), Err(WssError::CreditCounterExhausted));
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (0, 0)
    );
    assert_eq!(credit.consumed_frames, 1);
}

#[test]
fn maximum_frame_count_is_terminal_for_a_valid_positive_charge_history() {
    let mut credit = ReceiverCreditWindow::new(7, 8, 2).unwrap();
    // A long-running epoch has already released all but its last one-byte frame.
    credit.consumed_bytes = u64::MAX - 1;
    credit.consumed_frames = u64::MAX - 1;
    credit.released_bytes = u64::MAX - 2;
    credit.released_frames = u64::MAX - 2;
    credit.try_reserve(1).unwrap();
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (2, 2)
    );
    credit
        .return_credit(release(7, u64::MAX, u64::MAX))
        .unwrap();
    // Each frame costs at least one byte, so consumed_frames <= consumed_bytes.
    // Reaching the frame maximum necessarily also reaches the byte maximum:
    // the earlier byte-overflow check must refuse this valid terminal state.
    assert_eq!(credit.try_reserve(1), Err(WssError::CreditCounterExhausted));
    assert_eq!(
        (credit.outstanding_bytes(), credit.outstanding_frames()),
        (0, 0)
    );
    assert_eq!(
        (credit.consumed_bytes, credit.consumed_frames),
        (u64::MAX, u64::MAX)
    );
}
