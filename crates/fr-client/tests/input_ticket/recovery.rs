//! A reference recovery changes ticket authority, not the lease or action log.
use super::*;

fn recovered_wire(
    generation: RecoveryGeneration,
    id: u128,
    sequence: u64,
    issued: u64,
    expires: u64,
) -> Vec<u8> {
    let mut c = credentials();
    c.ticket = InputTicketId::from_raw(id);
    c.view.recovery = generation;
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
fn initialized() -> InputClient {
    let mut v = setup();
    v.accept_ticket(
        &wire(5, 0, 1_000_000, 2_000_000),
        clock(),
        ClientInstant(10_100),
    )
    .unwrap();
    v
}
fn recovering(v: &mut InputClient) -> RecoveryGeneration {
    let next = RecoveryGeneration::INITIAL.next().unwrap();
    v.suspend(ClientInstant(20_000));
    v.advance_recovery(next, ClientInstant(20_000)).unwrap();
    next
}
fn show(v: &mut InputClient, generation: RecoveryGeneration, at: u64) {
    let mut c = credentials();
    c.view.recovery = generation;
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
fn issue(v: &mut InputClient, generation: RecoveryGeneration, id: u128, sequence: u64) {
    v.accept_ticket(
        &recovered_wire(generation, id, sequence, 1_020_000, 2_000_000),
        clock(),
        ClientInstant(30_100),
    )
    .unwrap();
}

#[test]
fn a_new_generation_ticket_resumes_a_fresh_recovered_view_without_an_extra_second() {
    let mut v = initialized();
    let next = recovering(&mut v);
    show(&mut v, next, 30_000);
    assert_eq!(
        v.ticket_deadline(),
        None,
        "the old view's ticket is not current"
    );
    assert_eq!(
        v.action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(30_000)
        ),
        Err(Error::ViewSuspended)
    );
    issue(&mut v, next, 6, 1);
    assert_eq!(v.suspended_since(), None);
    assert_eq!(v.ticket_deadline(), Some(ClientInstant(1_010_000)));
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let sent = v.action(press(), &mut out, ClientInstant(30_101)).unwrap();
    let request = parse(&out[..sent.bytes]);
    assert_eq!(request.credentials.lease, credentials().lease);
    assert_eq!(request.credentials.ticket, InputTicketId::from_raw(6));
    assert_eq!(request.credentials.view.recovery, next);
    assert_eq!(sent.sequence, 0);
}

#[test]
fn a_recovery_ticket_without_fresh_presentation_still_cannot_send() {
    let mut v = initialized();
    let next = recovering(&mut v);
    issue(&mut v, next, 6, 1);
    assert_eq!(v.suspended_since(), Some(ClientInstant(20_000)));
    assert_eq!(
        v.action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(30_101)
        ),
        Err(Error::ViewSuspended)
    );
    assert_eq!(v.pending_actions(), 0);
    show(&mut v, next, 30_102);
    assert_eq!(v.suspended_since(), None);
    assert!(
        v.action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(30_102)
        )
        .is_ok()
    );
}

#[test]
fn an_expired_recovery_ticket_cannot_resume_but_the_next_timely_one_can() {
    let mut v = initialized();
    let next = recovering(&mut v);
    show(&mut v, next, 30_000);
    assert_eq!(
        v.accept_ticket(
            &recovered_wire(next, 6, 1, 1_010_000, 1_020_000),
            clock(),
            ClientInstant(30_000),
        ),
        Err(Error::TicketExpired)
    );
    assert_eq!(v.suspended_since(), Some(ClientInstant(20_000)));
    assert_eq!(v.ticket_deadline(), None);
    issue(&mut v, next, 7, 2);
    assert_eq!(v.suspended_since(), None);
    assert_eq!(v.stopped(), None);
}

#[test]
fn ticket_replay_high_water_survives_the_recovery_boundary_and_expiry() {
    for sequence in [0, 1] {
        let mut v = initialized();
        let next = recovering(&mut v);
        if sequence == 1 {
            assert_eq!(
                v.accept_ticket(
                    &recovered_wire(next, 6, 1, 1_010_000, 1_020_000),
                    clock(),
                    ClientInstant(30_000),
                ),
                Err(Error::TicketExpired)
            );
        }
        assert_eq!(
            v.accept_ticket(
                &recovered_wire(next, 7, sequence, 1_020_000, 2_000_000),
                clock(),
                ClientInstant(30_100),
            ),
            Err(Error::Stopped(StopReason::InvalidTicket))
        );
    }
}

#[test]
fn recovery_does_not_reset_pending_receipts_or_replay_held_input() {
    let mut v = initialized();
    let mut out = [0; MAX_INPUT_RECORD_BYTES];
    let first = v.action(press(), &mut out, ClientInstant(10_101)).unwrap();
    assert_eq!(first.sequence, 0);
    assert!(v.holds_key(PhysicalKey::new(4).unwrap()));
    let next = recovering(&mut v);
    assert!(!v.holds_key(PhysicalKey::new(4).unwrap()));
    show(&mut v, next, 30_000);
    issue(&mut v, next, 6, 1);
    assert_eq!(v.pending_actions(), 1);
    assert!(!v.holds_key(PhysicalKey::new(4).unwrap()));
    let second = v.action(press(), &mut out, ClientInstant(30_101)).unwrap();
    assert_eq!(second.sequence, 1);
    assert_eq!(v.pending_actions(), 2);
}

#[test]
fn repeated_recovery_requires_another_ticket_without_restarting_the_suspension_budget() {
    let mut v = initialized();
    let next = recovering(&mut v);
    let following = next.next().unwrap();
    v.advance_recovery(following, ClientInstant(25_000))
        .unwrap();
    assert_eq!(v.suspended_since(), Some(ClientInstant(20_000)));
    show(&mut v, following, 30_000);
    issue(&mut v, following, 6, 1);
    assert_eq!(v.suspended_since(), None);
    assert_eq!(v.suspension_totals(ClientInstant(30_100)), (1, 10_100));
}

#[test]
fn recovery_before_the_first_picture_cannot_reuse_an_initial_ticket() {
    let c = credentials();
    let mut v = InputClient::new(
        c,
        7,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 100, 100).unwrap(),
        Capabilities::default().with(Capability::Keys),
        ProtocolLimits::ABSOLUTE,
        Policy::default(),
        ClientInstant(10_100),
    )
    .unwrap();
    v.confirm_mapping(c.session, c.view, ClientInstant(10_100))
        .unwrap();
    v.accept_ticket(
        &wire(5, 0, 1_000_000, 2_000_000),
        clock(),
        ClientInstant(10_100),
    )
    .unwrap();
    let next = c.view.recovery.next().unwrap();
    v.advance_recovery(next, ClientInstant(20_000)).unwrap();
    show(&mut v, next, 30_000);
    assert_eq!(v.ticket_deadline(), None);
    assert_eq!(
        v.action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(30_000)
        ),
        Err(Error::TicketExpired)
    );
    issue(&mut v, next, 6, 1);
    assert!(
        v.action(
            press(),
            &mut [0; MAX_INPUT_RECORD_BYTES],
            ClientInstant(30_101)
        )
        .is_ok()
    );
}
