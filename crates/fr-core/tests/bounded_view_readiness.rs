//! Expiry at the original source deadline, independent of reactor scheduling.
use fr_core::{
    authority::{AuthorityError, AuthorityPolicy, SessionAuthority, ViewReadiness},
    ids::{InputLeaseId, InputTicketId, RemoteSessionId},
    time::HostInstant,
};
fn at(t: u64) -> HostInstant {
    HostInstant::from_micros(t)
}
fn opening() -> SessionAuthority {
    let mut a = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    a.mark_capabilities_checked().unwrap();
    a.authorize_observation(at(0)).unwrap();
    a.require_view_evidence(at(0)).unwrap();
    a
}
fn granted() -> (SessionAuthority, InputLeaseId, InputTicketId) {
    let mut a = opening();
    let lease = InputLeaseId::from_raw(2);
    let ticket = InputTicketId::from_raw(3);
    a.mark_view_ready_until(at(250_000), at(100_000)).unwrap();
    a.grant_lease(lease, at(100_000)).unwrap();
    a.issue_input_ticket(lease, ticket, at(100_000)).unwrap();
    (a, lease, ticket)
}
#[test]
fn source_expiry_fences_submission_without_a_network_or_watchdog_tick() {
    let (mut a, lease, ticket) = granted();
    assert_eq!(a.control_deadline().unwrap(), at(250_000));
    a.authorize_submission(lease, ticket, at(249_999)).unwrap();
    assert_eq!(
        a.authorize_submission(lease, ticket, at(250_000)),
        Err(AuthorityError::ViewUnready)
    );
    assert_eq!(a.readiness(), ViewReadiness::Stale);
    assert!(a.control_deadline().is_err());
    assert!(a.observation_deadline(at(250_000)).is_ok());
}
#[test]
fn late_evidence_cannot_revive_a_lease_even_before_the_first_expiry_tick() {
    let (mut a, lease, ticket) = granted();
    assert_eq!(
        a.mark_view_ready_until(at(500_000), at(250_000)),
        Err(AuthorityError::ViewUnready)
    );
    assert!(!a.has_live_control(at(250_000)));
    assert!(
        a.issue_input_ticket(lease, InputTicketId::from_raw(4), at(250_000))
            .is_err()
    );
    a.revoke_lease();
    a.mark_view_ready_until(at(500_000), at(250_001)).unwrap();
    let next = InputLeaseId::from_raw(5);
    a.grant_lease(next, at(250_001)).unwrap();
    assert_eq!(
        a.authorize_submission(lease, ticket, at(250_002)),
        Err(AuthorityError::StaleLease)
    );
}
#[test]
fn newer_source_preserves_existing_tickets_but_does_not_renew_the_lease() {
    let (mut a, lease, ticket) = granted();
    let lease_until = a.lease_deadline().unwrap();
    a.mark_view_ready_until(at(400_000), at(200_000)).unwrap();
    assert_eq!(a.control_deadline().unwrap(), at(400_000));
    assert_eq!(a.lease_deadline().unwrap(), lease_until);
    a.authorize_submission(lease, ticket, at(300_000)).unwrap();
    assert!(a.authorize_submission(lease, ticket, at(400_000)).is_err());
}
#[test]
fn observation_and_control_heartbeats_cannot_extend_view_readiness() {
    let (mut a, lease, ticket) = granted();
    a.issue_observation_challenge(10, at(120_000)).unwrap();
    a.respond_observation_challenge(10, at(130_000)).unwrap();
    a.issue_control_challenge(11, at(140_000)).unwrap();
    a.respond_control_challenge(lease, 11, at(150_000)).unwrap();
    assert_eq!(a.control_deadline().unwrap(), at(250_000));
    assert!(a.authorize_submission(lease, ticket, at(250_000)).is_err());
}
#[test]
fn repeated_or_regressing_source_deadlines_never_gain_receipt_time() {
    let mut a = opening();
    a.mark_view_ready_until(at(250_000), at(200_000)).unwrap();
    a.mark_view_ready_until(at(250_000), at(240_000)).unwrap();
    assert!(a.mark_view_ready_until(at(240_000), at(241_000)).is_err());
    assert!(a.mark_view_ready_until(at(249_000), at(242_000)).is_err());
    assert!(a.mark_view_ready_until(at(250_000), at(250_000)).is_err());
    assert_eq!(a.readiness(), ViewReadiness::Stale);
}
#[test]
fn finite_mode_cannot_be_disabled_by_legacy_setter_or_suspend() {
    let mut a = opening();
    assert!(a.mark_view_ready(at(0)).is_err());
    a.mark_view_ready_until(at(250_000), at(1)).unwrap();
    a.invalidate_for_suspend();
    a.authorize_observation(at(2)).unwrap();
    assert!(a.mark_view_ready(at(2)).is_err());
    a.mark_view_ready_until(at(250_000), at(3)).unwrap();
    a.close();
    assert!(a.mark_view_ready_until(at(500_000), at(4)).is_err());
}

#[test]
fn independent_native_monitor_fences_even_without_any_authority_mutation() {
    use fr_core::{ids::*, input::*, input_submission::*};
    let (a, lease, ticket) = granted();
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease,
        ticket,
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut owner = InputSession::new(
        a,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default().with(Capability::Keys),
        at(100_000),
    )
    .unwrap();
    let monitor = owner.monitor();
    let mut renewal = owner.take_control_lease().unwrap();
    assert_eq!(monitor.deadline(at(249_999)).unwrap(), at(250_000));
    assert_eq!(renewal.deadline(at(249_999)).unwrap(), at(250_000));
    assert!(monitor.deadline(at(250_000)).is_err());
    assert!(monitor.is_revoked());
    assert!(renewal.respond(17, at(250_000)).is_err());
    assert!(
        owner
            .issue_ticket(InputTicketId::from_raw(99), at(250_000))
            .is_err()
    );
}
