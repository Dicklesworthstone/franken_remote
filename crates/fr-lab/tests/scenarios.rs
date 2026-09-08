#![forbid(unsafe_code)]

use fr_core::authority::{AuthorityError, AuthorityPolicy, SessionAuthority};
use fr_core::ids::{InputLeaseId, InputTicketId, RemoteSessionId};
use fr_core::time::{HostDuration, HostInstant};
use fr_lab::{Destination, Fault, Limits, Scenario};

fn scenario(seed: u64, limits: Limits) -> Result<Scenario, fr_lab::Failure> {
    eprintln!("fr-lab seed={seed}");
    Scenario::new(seed, limits)
}

fn ms(value: u64) -> HostDuration {
    HostDuration::from_millis_checked(value).unwrap()
}

fn controlling() -> (SessionAuthority, InputLeaseId, InputTicketId) {
    let mut state = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    state.mark_capabilities_checked().unwrap();
    state.authorize_observation(HostInstant::ORIGIN).unwrap();
    state.mark_view_ready(HostInstant::ORIGIN).unwrap();
    let lease = InputLeaseId::from_raw(2);
    let ticket = InputTicketId::from_raw(3);
    state.grant_lease(lease, HostInstant::ORIGIN).unwrap();
    state
        .issue_input_ticket(lease, ticket, HostInstant::ORIGIN)
        .unwrap();
    (state, lease, ticket)
}

#[test]
fn connect_approval_view_control_and_close_drive_production_transitions() {
    let mut lab = scenario(16, Limits::default()).unwrap();
    let mut host = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    let lease = InputLeaseId::from_raw(2);
    // These bytes select test actions; they are deliberately not fr-wire records.
    lab.send(Destination::Host, &[1], Fault::after(ms(5)))
        .unwrap();
    lab.elapse(ms(5)).unwrap();
    lab.drain(|delivery| {
        assert_eq!(delivery.payload, &[1], "seed=16");
        host.mark_capabilities_checked().unwrap();
        host.require_approval().unwrap();
        0
    })
    .unwrap();
    assert!(
        host.mark_view_ready(lab.now()).is_err(),
        "approval bypass: {lab:?}"
    );
    host.authorize_observation(lab.now()).unwrap(); // explicit local approval
    host.mark_view_ready(lab.now()).unwrap();
    assert!(
        !host.has_live_control(lab.now()),
        "frame granted control: {lab:?}"
    );
    lab.send(Destination::Host, &[2], Fault::after(ms(1)))
        .unwrap();
    lab.elapse(ms(1)).unwrap();
    lab.drain(|delivery| {
        host.grant_lease(lease, delivery.now).unwrap();
        0
    })
    .unwrap();
    lab.send(Destination::Client, &[3], Fault::after(ms(1)))
        .unwrap();
    lab.elapse(ms(1)).unwrap();
    let mut client_saw_grant = false;
    lab.drain(|delivery| {
        assert_eq!(delivery.to, Destination::Client, "seed=16");
        client_saw_grant = delivery.payload == [3];
        0
    })
    .unwrap();
    assert!(
        client_saw_grant && host.has_live_control(lab.now()),
        "{lab:?}"
    );
    host.close(); // local authority fence always precedes transport cleanup
    lab.fence().unwrap();
    assert!(!host.has_live_control(lab.now()), "{lab:?}");
}

#[test]
fn healthy_delivery_authorizes_before_the_ticket_deadline() {
    let mut lab = scenario(15, Limits::default()).unwrap();
    let (state, lease, ticket) = controlling();
    lab.send(
        Destination::Host,
        b"synthetic-action",
        Fault::after(ms(100)),
    )
    .unwrap();
    let mut result = None;
    lab.advance(ms(1_500), |delivery| {
        result = Some((
            delivery.now,
            state.authorize_submission(lease, ticket, delivery.now),
        ));
        0
    })
    .unwrap();
    assert_eq!(
        result,
        Some((HostInstant::from_micros(100_000), Ok(()))),
        "{lab:?}"
    );
    assert_eq!(lab.now(), HostInstant::from_micros(1_500_000), "{lab:?}");
}

#[test]
fn stalled_delivery_checks_the_actual_submission_clock() {
    let mut lab = scenario(17, Limits::default()).unwrap();
    let (state, lease, ticket) = controlling();
    lab.send(
        Destination::Host,
        b"synthetic-action",
        Fault::after(ms(100)),
    )
    .unwrap();
    // Bytes were due at 100ms, but the authority task does not run until 1500ms.
    lab.elapse(ms(1_500)).unwrap();
    let mut result = None;
    lab.drain(|delivery| {
        result = Some(state.authorize_submission(lease, ticket, delivery.now));
        0
    })
    .unwrap();
    assert_eq!(result, Some(Err(AuthorityError::TicketExpired)), "{lab:?}");
    assert_eq!(lab.metrics().queued_packets, 0, "{lab:?}");
}

#[test]
fn resume_fences_authority_before_old_input_is_drained() {
    let mut lab = scenario(18, Limits::default()).unwrap();
    let (mut state, lease, ticket) = controlling();
    lab.send(Destination::Host, b"synthetic-action", Fault::after(ms(1)))
        .unwrap();
    lab.elapse(ms(4_000)).unwrap();
    assert!(!state.apply_resume_boundary(lab.now()), "{lab:?}");
    let mut admitted = 0;
    lab.drain(|delivery| {
        if state
            .authorize_submission(lease, ticket, delivery.now)
            .is_ok()
        {
            admitted += 1;
        }
        0
    })
    .unwrap();
    assert_eq!(admitted, 0, "{lab:?}");
}

#[test]
fn late_observation_response_cannot_resurrect_expired_observation() {
    let mut lab = scenario(19, Limits::default()).unwrap();
    let (mut state, _, _) = controlling();
    lab.elapse(ms(2_000)).unwrap();
    let _deadline = state.issue_observation_challenge(55, lab.now());
    lab.send(
        Destination::Host,
        b"synthetic-response",
        Fault::after(ms(2_000)),
    )
    .unwrap();
    lab.elapse(ms(2_000)).unwrap();
    let mut result = None;
    lab.drain(|delivery| {
        result = Some(state.respond_observation_challenge(55, delivery.now));
        0
    })
    .unwrap();
    assert!(
        result.unwrap().is_err(),
        "expired observation resurrected: {lab:?}"
    );
}

#[test]
fn renewed_control_does_not_bypass_observation_expiry() {
    let mut lab = scenario(20, Limits::default()).unwrap();
    let (mut state, lease, _) = controlling();
    lab.elapse(ms(2_500)).unwrap();
    state.issue_control_challenge(56, lab.now()).unwrap();
    state
        .respond_control_challenge(lease, 56, lab.now())
        .unwrap();
    let ticket = InputTicketId::from_raw(4);
    state.issue_input_ticket(lease, ticket, lab.now()).unwrap();
    lab.send(
        Destination::Host,
        b"synthetic-action",
        Fault::after(ms(600)),
    )
    .unwrap();
    lab.elapse(ms(600)).unwrap();
    let mut result = None;
    lab.drain(|delivery| {
        result = Some(state.authorize_submission(lease, ticket, delivery.now));
        0
    })
    .unwrap();
    assert!(
        result.unwrap().is_err(),
        "input outlived observation: {lab:?}"
    );
}
