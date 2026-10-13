//! Ticket feedback and media replacement travel on independently ordered lanes.
use super::*;

fn ticket_for(c: InputCredentials, sequence: u64, issued: u64, expires: u64) -> Vec<u8> {
    let mut bytes = vec![0; INPUT_TICKET_BYTES];
    input_ticket::encode(
        Ticket {
            credentials: c,
            sequence,
            issued_at_us: issued,
            expires_at_us: expires,
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        7,
        Direction::HostToViewer,
        Delivery::Reliable,
    )
    .unwrap();
    bytes
}
fn recovering() -> (InputClient, InputCredentials) {
    let mut v = setup();
    v.accept_ticket(
        &wire(5, 0, 1_000_000, 2_000_000),
        clock(),
        ClientInstant(10_100),
    )
    .unwrap();
    v.suspend(ClientInstant(20_000));
    let mut next = credentials();
    next.ticket = InputTicketId::from_raw(7);
    next.view.recovery = next.view.recovery.next().unwrap();
    v.advance_recovery(next.view.recovery, ClientInstant(20_000))
        .unwrap();
    (v, next)
}
fn present(v: &mut InputClient, c: InputCredentials, at: u64) {
    v.presented(
        PresentedObservation {
            session: c.session,
            serial: 0,
            view: c.view,
            received_at: ClientInstant(at),
            source_age_upper_us: 0,
        },
        ClientInstant(at),
    )
    .unwrap();
}

#[test]
fn queued_old_tickets_are_drained_without_authority_before_the_new_generation_ticket() {
    for old_expires in [1_010_000, 2_000_000] {
        for picture_first in [false, true] {
            let (mut v, next) = recovering();
            if picture_first {
                present(&mut v, next, 30_000);
            }
            // This record was queued before the host's recovery fence; its
            // feedback stream is independent of the replacement handshake.
            assert_eq!(
                v.accept_ticket(
                    &wire(6, 1, 1_001_000, old_expires),
                    clock(),
                    ClientInstant(30_100),
                ),
                Ok(())
            );
            assert_eq!(v.stopped(), None);
            assert_eq!(v.ticket_deadline(), None);
            assert_eq!(v.suspended_since(), Some(ClientInstant(20_000)));
            let mut out = [0; MAX_INPUT_RECORD_BYTES];
            assert_eq!(
                v.action(press(), &mut out, ClientInstant(30_101)),
                Err(Error::ViewSuspended)
            );
            assert_eq!(v.pending_actions(), 0);
            v.accept_ticket(
                &ticket_for(next, 2, 1_020_000, 2_000_000),
                clock(),
                ClientInstant(30_102),
            )
            .unwrap();
            if !picture_first {
                assert_eq!(v.suspended_since(), Some(ClientInstant(20_000)));
                present(&mut v, next, 30_103);
            }
            let sent = v.action(press(), &mut out, ClientInstant(30_104)).unwrap();
            let c = parse(&out[..sent.bytes]).credentials;
            assert_eq!(c, next);
            assert_eq!(sent.sequence, 0);
        }
    }
}

#[test]
fn an_obsolete_ticket_still_consumes_its_replay_sequence() {
    for replay_current in [false, true] {
        let (mut v, next) = recovering();
        let old = wire(6, 1, 1_001_000, 2_000_000);
        v.accept_ticket(&old, clock(), ClientInstant(30_100))
            .unwrap();
        let replay = if replay_current {
            ticket_for(next, 1, 1_020_000, 2_000_000)
        } else {
            old
        };
        assert_eq!(
            v.accept_ticket(&replay, clock(), ClientInstant(30_101)),
            Err(Error::Stopped(StopReason::InvalidTicket))
        );
    }
}

#[test]
fn obsolete_feedback_does_not_waive_other_authority_or_display_bindings() {
    for field in 0..6 {
        let (mut v, _) = recovering();
        let mut c = credentials();
        c.ticket = InputTicketId::from_raw(6);
        match field {
            0 => c.session = RemoteSessionId::from_raw(9),
            1 => c.lease = InputLeaseId::from_raw(9),
            2 => c.view.geometry = c.view.geometry.next().unwrap(),
            3 => c.view.viewport = c.view.viewport.next().unwrap(),
            4 => c.view.configuration = c.view.configuration.next().unwrap(),
            _ => c.view.recovery = RecoveryGeneration::from_raw(2),
        }
        assert_eq!(
            v.accept_ticket(
                &ticket_for(c, 1, 1_001_000, 2_000_000),
                clock(),
                ClientInstant(30_100),
            ),
            Err(Error::Stopped(StopReason::InvalidTicket)),
            "field {field}"
        );
    }
}

#[test]
fn the_ticket_lane_cannot_go_back_to_an_old_generation_after_a_current_ticket() {
    let (mut v, next) = recovering();
    v.accept_ticket(
        &ticket_for(next, 1, 1_020_000, 2_000_000),
        clock(),
        ClientInstant(30_100),
    )
    .unwrap();
    assert_eq!(
        v.accept_ticket(
            &wire(8, 2, 1_020_001, 2_000_000),
            clock(),
            ClientInstant(30_102),
        ),
        Err(Error::Stopped(StopReason::InvalidTicket))
    );
}

#[test]
fn consumed_generation_high_water_also_applies_to_expired_and_obsolete_tickets() {
    for expired_current in [false, true] {
        let (mut v, mut next) = recovering();
        if !expired_current {
            // A second recovery can complete before this ordered feedback
            // stream has delivered its first recovery's queued ticket.
            next.view.recovery = next.view.recovery.next().unwrap();
            v.advance_recovery(next.view.recovery, ClientInstant(25_000))
                .unwrap();
        }
        let mut intermediate = next;
        intermediate.view.recovery = RecoveryGeneration::from_raw(1);
        let result = v.accept_ticket(
            &ticket_for(intermediate, 1, 1_010_000, 1_020_000),
            clock(),
            ClientInstant(30_100),
        );
        assert_eq!(
            result,
            if expired_current {
                Err(Error::TicketExpired)
            } else {
                Ok(())
            }
        );
        assert_eq!(v.ticket_deadline(), None);
        assert_eq!(
            v.accept_ticket(
                &wire(8, 2, 1_020_000, 2_000_000),
                clock(),
                ClientInstant(30_101),
            ),
            Err(Error::Stopped(StopReason::InvalidTicket))
        );
    }
}

#[test]
fn obsolete_feedback_still_rejects_regressed_or_future_issue_time() {
    for issued in [999_999, 1_040_000] {
        let (mut v, _) = recovering();
        assert_eq!(
            v.accept_ticket(
                &wire(6, 1, issued, 2_000_000),
                clock(),
                ClientInstant(30_100),
            ),
            Err(Error::Stopped(StopReason::InvalidTicket))
        );
    }
}

#[test]
fn draining_obsolete_feedback_cannot_extend_suspension_or_pending_receipt_deadlines() {
    let (mut v, _) = recovering();
    v.accept_ticket(
        &wire(6, 1, 1_001_000, 2_000_000),
        clock(),
        ClientInstant(30_100),
    )
    .unwrap();
    assert_eq!(
        v.tick(ClientInstant(
            20_000 + fr_client::input::MAX_VIEW_SUSPENSION_US
        )),
        Err(Error::Stopped(StopReason::ViewStale))
    );

    let mut v = setup();
    v.accept_ticket(
        &wire(5, 0, 1_000_000, 2_000_000),
        clock(),
        ClientInstant(10_100),
    )
    .unwrap();
    let sent = v
        .action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(10_101),
        )
        .unwrap();
    assert_eq!(sent.sequence, 0);
    v.suspend(ClientInstant(20_000));
    v.advance_recovery(RecoveryGeneration::from_raw(1), ClientInstant(20_000))
        .unwrap();
    v.accept_ticket(
        &wire(6, 1, 1_001_000, 2_000_000),
        clock(),
        ClientInstant(30_100),
    )
    .unwrap();
    assert_eq!(v.pending_actions(), 1);
    assert!(!v.holds_key(PhysicalKey::new(4).unwrap()));
    assert_eq!(
        v.tick(ClientInstant(2_010_101)),
        Err(Error::Stopped(StopReason::ReceiptTimeout))
    );
}

#[test]
fn a_second_recovery_before_presentation_keeps_the_original_suspension_deadline() {
    let (mut v, mut next) = recovering();
    v.accept_ticket(
        &ticket_for(next, 1, 1_020_000, 2_000_000),
        clock(),
        ClientInstant(30_100),
    )
    .unwrap();
    // Ticket alone did not resume. Another recovery must not turn a new
    // picture plus the previous generation's ticket into a resumed view.
    next.view.recovery = next.view.recovery.next().unwrap();
    v.advance_recovery(next.view.recovery, ClientInstant(30_101))
        .unwrap();
    present(&mut v, next, 30_102);
    assert_eq!(v.ticket_deadline(), None);
    assert_eq!(v.suspended_since(), Some(ClientInstant(20_000)));
    next.ticket = InputTicketId::from_raw(8);
    v.accept_ticket(
        &ticket_for(next, 2, 1_030_000, 2_000_000),
        clock(),
        ClientInstant(40_100),
    )
    .unwrap();
    assert_eq!(v.suspended_since(), None);
    assert_eq!(v.suspension_totals(ClientInstant(40_100)), (1, 20_100));
}
