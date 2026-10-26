//! Expiry at the original source deadline, independent of reactor scheduling.
use fr_core::{
    authority::{
        AuthorityError, AuthorityPolicy, ControlStatus, MAX_VIEW_SUSPENSION, SessionAuthority,
        ViewReadiness,
    },
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
fn late_evidence_cannot_revive_a_suspended_lease_before_its_held_input_is_released() {
    // Plan 11.3: a stale view SUSPENDS input under a held lease. Even before
    // the first expiry tick, fresh evidence cannot revive it until the native
    // owner confirms every remotely held key and button was released.
    let (mut a, lease, ticket) = granted();
    let lease_until = a.lease_deadline().unwrap();
    assert_eq!(
        a.mark_view_ready_until(at(500_000), at(250_000)),
        Err(AuthorityError::ReleasePending)
    );
    assert!(!a.has_live_control(at(250_000)));
    assert!(
        a.issue_input_ticket(lease, InputTicketId::from_raw(4), at(250_000))
            .is_err()
    );
    assert_eq!(
        a.control_status(at(250_000)),
        Ok(ControlStatus::Suspended {
            until: lease_until,
            released: false
        })
    );
    // Only the lease's own owner confirms the release.
    assert_eq!(
        a.confirm_suspension_release(InputLeaseId::from_raw(9)),
        Err(AuthorityError::StaleLease)
    );
    a.confirm_suspension_release(lease).unwrap();
    // Fresh evidence now revives input, but neither the old ticket nor the
    // lease lifetime: a new ticket is required and the lease is not renewed.
    a.mark_view_ready_until(at(500_000), at(250_001)).unwrap();
    assert_eq!(
        a.control_status(at(250_001)),
        Ok(ControlStatus::Live { until: at(500_000) })
    );
    assert_eq!(
        a.authorize_submission(lease, ticket, at(250_002)),
        Err(AuthorityError::TicketInvalid)
    );
    let fresh = InputTicketId::from_raw(4);
    a.issue_input_ticket(lease, fresh, at(250_002)).unwrap();
    a.authorize_submission(lease, fresh, at(250_003)).unwrap();
    assert_eq!(a.lease_deadline().unwrap(), lease_until);
}
#[test]
fn a_revoked_suspended_lease_is_replaced_only_by_a_new_grant() {
    let (mut a, lease, ticket) = granted();
    assert_eq!(
        a.mark_view_ready_until(at(500_000), at(250_000)),
        Err(AuthorityError::ReleasePending)
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
    // Renewal follows the lease, not the view (b8adb51); input submission is
    // still bounded by the view through the monitor.
    assert_eq!(renewal.deadline(at(249_999)).unwrap(), at(3_000_000));
    // The view deadline passes with no authority mutation: input is refused
    // and SUSPENDED, but the lease is not revoked (plan 11.3).
    assert_eq!(
        monitor.deadline(at(250_000)),
        Err(Refusal::Authority(AuthorityError::ViewUnready))
    );
    assert!(!monitor.is_revoked());
    assert_eq!(
        monitor.status(at(250_000)),
        Ok(ControlStatus::Suspended {
            until: at(3_000_000),
            released: false
        })
    );
    assert!(
        owner
            .issue_ticket(InputTicketId::from_raw(99), at(250_000))
            .is_err()
    );
    // Renewal continues while suspended...
    assert_eq!(renewal.deadline(at(250_000)), Ok(at(3_000_000)));
    // ...and observation expiry during the suspension is still terminal.
    assert!(monitor.deadline(at(3_000_000)).is_err());
    assert!(monitor.is_revoked());
    assert!(renewal.respond(17, at(3_000_000)).is_err());
}
#[test]
fn a_suspension_that_outlasts_its_limit_ends_control_even_with_renewals() {
    let (mut a, lease, _) = granted();
    // The view lapses at 250 ms; observation and control keep being renewed.
    let limit = 250_000 + MAX_VIEW_SUSPENSION.as_micros();
    let (mut t, mut nonce) = (1_000_000, 100);
    loop {
        nonce += 2;
        a.issue_observation_challenge(nonce, at(t)).unwrap();
        a.respond_observation_challenge(nonce, at(t)).unwrap();
        a.issue_control_challenge(nonce + 1, at(t)).unwrap();
        a.respond_control_challenge(lease, nonce + 1, at(t))
            .unwrap();
        if t + 1_000_000 >= limit {
            break;
        }
        t += 1_000_000;
    }
    assert!(a.lease_deadline().is_err(), "readiness is not back");
    assert!(matches!(
        a.control_status(at(limit - 1)),
        Ok(ControlStatus::Suspended { until, released: false }) if until == at(limit)
    ));
    assert_eq!(
        a.control_status(at(limit)),
        Err(AuthorityError::ViewUnready)
    );
}

// A report can acquire the authority lock before the next native-watchdog turn.
// Keep both authorization scopes live without checking control_status at the
// final sample: presentation itself must enforce the original suspension cap.
fn renewed_suspension() -> (SessionAuthority, InputLeaseId, InputTicketId) {
    let (mut authority, lease, ticket) = granted();
    assert_eq!(
        authority.authorize_submission(lease, ticket, at(250_000)),
        Err(AuthorityError::ViewUnready)
    );
    for second in 1..=10_u64 {
        let now = at(second * 1_000_000);
        let nonce = u128::from(second) * 2;
        authority.issue_observation_challenge(nonce, now).unwrap();
        authority.respond_observation_challenge(nonce, now).unwrap();
        authority.issue_control_challenge(nonce + 1, now).unwrap();
        authority
            .respond_control_challenge(lease, nonce + 1, now)
            .unwrap();
    }
    (authority, lease, ticket)
}

#[test]
fn fresh_presented_evidence_cannot_win_a_race_with_suspension_expiry() {
    let limit = 250_000 + MAX_VIEW_SUSPENSION.as_micros();
    for now in [limit, limit + 1, limit + 500_000] {
        let (mut authority, lease, old_ticket) = renewed_suspension();
        authority.confirm_suspension_release(lease).unwrap();
        let sequence = std::num::NonZeroU64::new(17).unwrap();
        assert_eq!(
            authority.mark_presented_ready_until(sequence, at(now + 250_000), at(now)),
            Err(AuthorityError::ViewUnready),
            "a fresh report cannot erase the expired suspension at {now}"
        );
        assert_eq!(authority.readiness(), ViewReadiness::Stale);
        assert_eq!(authority.presented_sequence(), None);
        assert_eq!(authority.observation_deadline(at(now)), Ok(at(13_000_000)));
        assert_eq!(
            authority.authorize_submission(lease, old_ticket, at(now)),
            Err(AuthorityError::ViewUnready)
        );
        assert_eq!(
            authority.issue_presented_input_ticket(lease, InputTicketId::from_raw(9), at(now)),
            Err(AuthorityError::ViewUnready)
        );
        // Repeated reports, ordinary readiness and confirmed cleanup cannot
        // start another suspension window or bypass the same original fence.
        authority.confirm_suspension_release(lease).unwrap();
        assert_eq!(
            authority.mark_view_ready_until(at(now + 250_001), at(now + 1)),
            Err(AuthorityError::ViewUnready)
        );
        assert_eq!(
            authority.control_status(at(now + 1)),
            Err(AuthorityError::ViewUnready)
        );
    }
}

#[test]
fn suspension_expiry_is_terminal_even_when_native_release_is_still_pending() {
    let (mut authority, lease, _) = renewed_suspension();
    let limit = 250_000 + MAX_VIEW_SUSPENSION.as_micros();
    assert_eq!(
        authority.mark_view_ready_until(at(limit + 250_000), at(limit - 1)),
        Err(AuthorityError::ReleasePending)
    );
    assert_eq!(
        authority.mark_view_ready_until(at(limit + 250_000), at(limit)),
        Err(AuthorityError::ViewUnready)
    );
    authority.confirm_suspension_release(lease).unwrap();
    assert_eq!(
        authority.mark_view_ready_until(at(limit + 250_001), at(limit + 1)),
        Err(AuthorityError::ViewUnready)
    );
    assert_eq!(
        authority.grant_lease(InputLeaseId::from_raw(8), at(limit + 1)),
        Err(AuthorityError::ViewUnready)
    );
    // The original lease stays fenced until its owner explicitly revokes it;
    // only a separate grant after that cleanup may control the restored view.
    authority.revoke_lease();
    authority
        .mark_view_ready_until(at(limit + 250_002), at(limit + 2))
        .unwrap();
    authority
        .grant_lease(InputLeaseId::from_raw(8), at(limit + 2))
        .unwrap();
}

#[test]
fn confirmed_release_and_fresh_evidence_just_before_the_limit_resume_the_same_lease() {
    let (mut authority, lease, old_ticket) = renewed_suspension();
    let limit = 250_000 + MAX_VIEW_SUSPENSION.as_micros();
    authority.confirm_suspension_release(lease).unwrap();
    let sequence = std::num::NonZeroU64::new(17).unwrap();
    authority
        .mark_presented_ready_until(sequence, at(limit + 250_000), at(limit - 1))
        .unwrap();
    assert_eq!(authority.presented_sequence(), Some(sequence));
    assert_eq!(authority.lease_deadline(), Ok(at(13_000_000)));
    assert_eq!(
        authority.authorize_submission(lease, old_ticket, at(limit)),
        Err(AuthorityError::TicketInvalid)
    );
    let fresh = InputTicketId::from_raw(9);
    authority
        .issue_presented_input_ticket(lease, fresh, at(limit))
        .unwrap();
    authority
        .authorize_submission(lease, fresh, at(limit + 1))
        .unwrap();
    assert_eq!(
        authority.control_status(at(limit + 1)),
        Ok(ControlStatus::Live {
            until: at(limit + 250_000)
        })
    );
}

#[test]
fn an_unrepresentable_suspension_limit_cannot_be_erased_by_fresh_evidence() {
    let start = u64::MAX - 5_000_000;
    let mut authority = SessionAuthority::new(
        RemoteSessionId::from_raw(1),
        AuthorityPolicy::plan_defaults(),
    );
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(start)).unwrap();
    authority.require_view_evidence(at(start)).unwrap();
    authority
        .mark_view_ready_until(at(start + 100_000), at(start))
        .unwrap();
    let lease = InputLeaseId::from_raw(2);
    let ticket = InputTicketId::from_raw(3);
    authority.grant_lease(lease, at(start)).unwrap();
    authority
        .issue_input_ticket(lease, ticket, at(start))
        .unwrap();
    assert_eq!(
        authority.authorize_submission(lease, ticket, at(start + 100_000)),
        Err(AuthorityError::ViewUnready)
    );
    authority.confirm_suspension_release(lease).unwrap();
    assert_eq!(
        authority.mark_view_ready_until(at(start + 300_000), at(start + 200_000)),
        Err(AuthorityError::DeadlineOverflow)
    );
    assert_eq!(authority.readiness(), ViewReadiness::Stale);
    assert_eq!(
        authority.control_status(at(start + 200_000)),
        Err(AuthorityError::DeadlineOverflow)
    );
}
