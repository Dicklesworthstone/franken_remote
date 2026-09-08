//! Session authority: the state machine and the three independent authority
//! variables (plan sections 6.3, 7.1–7.3, 15.1).
//!
//! Observation permission, media/view readiness, and input authority are
//! **separate state variables**. Receiving a first frame never grants
//! control; a controller slot is committed only after authorization,
//! readiness, and cleanup of any previous controller. This module owns those
//! rules as pure, clock-injected logic: every operation takes the current
//! [`HostInstant`] as data, so the whole machine is deterministic and
//! testable with no runtime. Asupersync region ownership wraps this later in
//! the broker; it does not change these decisions.
//!
//! The load-bearing anti-resurrection rules, all tested below:
//!
//! - a challenge response renews only an *outstanding, unexpired* challenge —
//!   a heartbeat that finally arrives after expiry extends nothing;
//! - expired leases and tickets are terminal; reacquisition is a new grant,
//!   never a revival of the old identity;
//! - suspend/resume and scheduler stalls are authority-generation
//!   boundaries: the deadline is checked against the current instant before
//!   any queued work is processed, so a clock that jumped forward across a
//!   sleep cannot let a pre-suspend lease survive;
//! - `authorize_submission` is the exact check the input agent runs
//!   immediately before every OS submission.

use crate::ids::{InputLeaseId, InputTicketId, RemoteSessionId};
use crate::time::{HostDuration, HostInstant};

/// Lifetime policy for challenges, leases, and input tickets. Starting points
/// from the plan; all bounded, none a latency claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthorityPolicy {
    /// Provisional observation/control authorization lifetime granted per
    /// challenge response (plan section 6.3: three seconds).
    pub authorization_lifetime: HostDuration,
    /// Input-ticket lifetime (plan section 15.1: 0.5–1.5 s), always clamped
    /// to not outlive the lease's current authorization.
    pub ticket_lifetime: HostDuration,
}

impl AuthorityPolicy {
    /// The plan's provisional defaults (section 6.3 / 15.1).
    #[must_use]
    pub fn plan_defaults() -> Self {
        Self {
            authorization_lifetime: HostDuration::from_millis_checked(3_000)
                .expect("3s fits u64 micros"),
            ticket_lifetime: HostDuration::from_millis_checked(1_000).expect("1s fits u64 micros"),
        }
    }
}

/// The session lifecycle (plan section 7.1). Observation, readiness, and
/// control are tracked as separate fields on [`SessionAuthority`]; this enum
/// is the coarse phase, and illegal transitions are refused, not panicked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Transport peer identified; only bounded negotiation admitted.
    Identified,
    /// Capabilities checked; configuration selectable.
    CapabilitiesChecked,
    /// Awaiting local approval before any observation (approval mode only).
    WaitingApproval,
    /// Observation authorized; view opening.
    ViewOpening,
    /// Observation authorized and a view is usable.
    Viewing,
    /// Refused terminally.
    Refused,
    /// Fenced/closing; no new observation, presses, text, or transfer work.
    Closed,
}

/// Why an authority operation was refused. Typed, bounded, loggable — never
/// reflected peer text (plan section 18.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthorityError {
    /// The operation is not legal from the current phase.
    InvalidState {
        /// The phase the session was in.
        phase: Phase,
    },
    /// Observation authorization has lapsed (deadline passed).
    ObservationExpired,
    /// No control lease is currently held.
    NoLease,
    /// The referenced lease is not the current one (stale/replaced).
    StaleLease,
    /// The control lease has expired; reacquisition required.
    LeaseExpired,
    /// The view is not ready; control cannot be granted or exercised.
    ViewUnready,
    /// A controller slot is already held by another session.
    ControllerBusy,
    /// The referenced input ticket is unknown, replaced, or belongs to
    /// another lease.
    TicketInvalid,
    /// The input ticket has expired; delayed traffic gets no new lifetime.
    TicketExpired,
    /// No challenge is outstanding, or the response does not match it.
    ChallengeMismatch,
    /// The outstanding challenge already expired; expiry is terminal.
    ChallengeExpired,
    /// A time went backwards relative to a recorded deadline/origin — treated
    /// as a fault, never silently tolerated.
    ClockRegression,
    /// A deadline computation overflowed the clock domain.
    DeadlineOverflow,
}

/// Whether the local view is currently trustworthy enough to accept control
/// actions (plan sections 11.3, 7.3). Control requires `Ready`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewReadiness {
    /// No usable view yet.
    Unready,
    /// A current, presented, trustworthy view.
    Ready,
    /// The view was ready but presentation is now stale/unknown; control is
    /// suspended until it returns to `Ready`.
    Stale,
}

/// An outstanding host challenge for observation or control liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Challenge {
    nonce: u128,
    deadline: HostInstant,
}

/// The control lease and its current authorization deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Lease {
    id: InputLeaseId,
    /// Authorization deadline; renewed by a matching challenge response,
    /// terminal once passed.
    authorized_until: HostInstant,
    /// The one currently valid input ticket, if any.
    ticket: Option<Ticket>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Ticket {
    id: InputTicketId,
    expires_at: HostInstant,
}

/// The serialized authority owner for one remote session (plan section 7.1:
/// handoff and revoke are decided here, never by a check-then-set race across
/// tasks). One value per admitted session; the broker holds exactly one and
/// all authority operations funnel through it.
#[derive(Debug, Clone)]
pub struct SessionAuthority {
    session: RemoteSessionId,
    policy: AuthorityPolicy,
    phase: Phase,
    /// Observation authorization deadline (independent of control).
    observation_until: Option<HostInstant>,
    readiness: ViewReadiness,
    lease: Option<Lease>,
    observation_challenge: Option<Challenge>,
    control_challenge: Option<Challenge>,
}

impl SessionAuthority {
    /// Opens a freshly identified session. Approval, capability checks, and
    /// observation authorization follow through the transition methods.
    #[must_use]
    pub fn new(session: RemoteSessionId, policy: AuthorityPolicy) -> Self {
        Self {
            session,
            policy,
            phase: Phase::Identified,
            observation_until: None,
            readiness: ViewReadiness::Unready,
            lease: None,
            observation_challenge: None,
            control_challenge: None,
        }
    }

    /// The session identity.
    #[must_use]
    pub fn session(&self) -> RemoteSessionId {
        self.session
    }

    /// The current coarse phase.
    #[must_use]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// The current view readiness.
    #[must_use]
    pub fn readiness(&self) -> ViewReadiness {
        self.readiness
    }

    /// Advances negotiation. `Identified -> CapabilitiesChecked`.
    pub fn mark_capabilities_checked(&mut self) -> Result<(), AuthorityError> {
        match self.phase {
            Phase::Identified => {
                self.phase = Phase::CapabilitiesChecked;
                Ok(())
            }
            phase => Err(AuthorityError::InvalidState { phase }),
        }
    }

    /// Enters the approval-pending phase (approval mode only). Legal from
    /// `CapabilitiesChecked`.
    pub fn require_approval(&mut self) -> Result<(), AuthorityError> {
        match self.phase {
            Phase::CapabilitiesChecked => {
                self.phase = Phase::WaitingApproval;
                Ok(())
            }
            phase => Err(AuthorityError::InvalidState { phase }),
        }
    }

    /// Grants observation authorization and opens the view, recording the
    /// first authorization deadline. Legal from `CapabilitiesChecked` (no
    /// approval) or `WaitingApproval` (approval granted). This is the single
    /// gate that admits pixels/thumbnails/audio/clipboard/semantic data:
    /// read-only observation is never a bypass of it (plan section 2.1).
    pub fn authorize_observation(&mut self, now: HostInstant) -> Result<(), AuthorityError> {
        match self.phase {
            Phase::CapabilitiesChecked | Phase::WaitingApproval => {
                self.observation_until = Some(self.deadline_from(now)?);
                self.phase = Phase::ViewOpening;
                Ok(())
            }
            phase => Err(AuthorityError::InvalidState { phase }),
        }
    }

    /// Reports that a usable, trustworthy view is now presented.
    /// `ViewOpening -> Viewing`, and readiness becomes `Ready`.
    pub fn mark_view_ready(&mut self, now: HostInstant) -> Result<(), AuthorityError> {
        self.check_observation_live(now)?;
        match self.phase {
            Phase::ViewOpening | Phase::Viewing => {
                self.phase = Phase::Viewing;
                self.readiness = ViewReadiness::Ready;
                Ok(())
            }
            phase => Err(AuthorityError::InvalidState { phase }),
        }
    }

    /// Reports sustained unknown/stale presentation. Readiness drops to
    /// `Stale`, which suspends control-action authorization until a fresh
    /// [`mark_view_ready`](Self::mark_view_ready) — without revoking the lease
    /// identity itself (plan section 11.3). Read-only status may continue.
    pub fn mark_view_stale(&mut self) {
        if self.readiness == ViewReadiness::Ready {
            self.readiness = ViewReadiness::Stale;
        }
    }

    /// Issues an observation liveness challenge on the host clock.
    #[must_use]
    pub fn issue_observation_challenge(&mut self, nonce: u128, now: HostInstant) -> HostInstant {
        let deadline = self
            .deadline_from(now)
            .unwrap_or(HostInstant::from_micros(u64::MAX));
        self.observation_challenge = Some(Challenge { nonce, deadline });
        deadline
    }

    /// Answers an outstanding observation challenge. Renews observation
    /// authorization only when the response matches the *current* challenge
    /// and arrives before that challenge's deadline: a delayed heartbeat
    /// never obtains a fresh lifetime at arrival (plan section 6.3).
    pub fn respond_observation_challenge(
        &mut self,
        nonce: u128,
        now: HostInstant,
    ) -> Result<HostInstant, AuthorityError> {
        // A challenge can only renew authority that is still live. If the
        // observation deadline already passed, authority is gone and
        // reacquisition is a new grant — a challenge issued while observation
        // was live cannot resurrect it after it lapsed (found in review by
        // AzureBasin; plan section 6.3).
        self.check_observation_live(now)?;
        let challenge = self
            .observation_challenge
            .ok_or(AuthorityError::ChallengeMismatch)?;
        if challenge.nonce != nonce {
            return Err(AuthorityError::ChallengeMismatch);
        }
        if now > challenge.deadline {
            // Expiry is terminal; do not resurrect on a late response.
            self.observation_challenge = None;
            return Err(AuthorityError::ChallengeExpired);
        }
        let renewed = self.deadline_from(now)?;
        self.observation_until = Some(renewed);
        self.observation_challenge = None;
        Ok(renewed)
    }

    /// Grants the control lease. Requires an authorized, live observation, a
    /// `Ready` view, and an *available* controller slot — a slot already held
    /// by a live lease is [`ControllerBusy`](AuthorityError::ControllerBusy);
    /// takeover goes through [`revoke_lease`](Self::revoke_lease) +
    /// [`grant_lease`](Self::grant_lease), serialized here, never a race
    /// (plan section 7.1, 7.3). Receiving frames does not reach this path.
    pub fn grant_lease(
        &mut self,
        lease_id: InputLeaseId,
        now: HostInstant,
    ) -> Result<(), AuthorityError> {
        self.check_observation_live(now)?;
        if self.phase != Phase::Viewing {
            return Err(AuthorityError::InvalidState { phase: self.phase });
        }
        if self.readiness != ViewReadiness::Ready {
            return Err(AuthorityError::ViewUnready);
        }
        if let Some(existing) = self.lease {
            // A still-live lease occupies the single controller slot.
            if now <= existing.authorized_until {
                return Err(AuthorityError::ControllerBusy);
            }
        }
        self.lease = Some(Lease {
            id: lease_id,
            authorized_until: self.deadline_from(now)?,
            ticket: None,
        });
        self.control_challenge = None;
        Ok(())
    }

    /// Issues a control liveness challenge for the current lease.
    pub fn issue_control_challenge(
        &mut self,
        nonce: u128,
        now: HostInstant,
    ) -> Result<HostInstant, AuthorityError> {
        let lease = self.lease.ok_or(AuthorityError::NoLease)?;
        if now > lease.authorized_until {
            return Err(AuthorityError::LeaseExpired);
        }
        let deadline = self.deadline_from(now)?;
        self.control_challenge = Some(Challenge { nonce, deadline });
        Ok(deadline)
    }

    /// Answers a control challenge, renewing the lease's authorization. Same
    /// anti-resurrection rule as observation: only a matching, unexpired
    /// challenge renews, and an already-expired lease cannot be revived by a
    /// late response (plan sections 6.3, 7.3).
    pub fn respond_control_challenge(
        &mut self,
        lease_id: InputLeaseId,
        nonce: u128,
        now: HostInstant,
    ) -> Result<HostInstant, AuthorityError> {
        // Control cannot outlive observation: if the observation deadline
        // lapsed, the control lease cannot be renewed either.
        self.check_observation_live(now)?;
        let lease = self.lease.as_mut().ok_or(AuthorityError::NoLease)?;
        if lease.id != lease_id {
            return Err(AuthorityError::StaleLease);
        }
        if now > lease.authorized_until {
            self.control_challenge = None;
            return Err(AuthorityError::LeaseExpired);
        }
        let challenge = self
            .control_challenge
            .ok_or(AuthorityError::ChallengeMismatch)?;
        if challenge.nonce != nonce {
            return Err(AuthorityError::ChallengeMismatch);
        }
        if now > challenge.deadline {
            self.control_challenge = None;
            return Err(AuthorityError::ChallengeExpired);
        }
        let renewed = now
            .checked_add(self.policy.authorization_lifetime)
            .ok_or(AuthorityError::DeadlineOverflow)?;
        lease.authorized_until = renewed;
        self.control_challenge = None;
        Ok(renewed)
    }

    /// Issues a fresh input ticket for the current lease, clamped to not
    /// outlive the lease's current authorization (plan section 15.1). Renewed
    /// at a cadence shorter than its lifetime without a per-action round trip;
    /// replacing a ticket invalidates the previous one.
    pub fn issue_input_ticket(
        &mut self,
        lease_id: InputLeaseId,
        ticket_id: InputTicketId,
        now: HostInstant,
    ) -> Result<HostInstant, AuthorityError> {
        // Control readiness gates ticket issuance: a stale view must not keep
        // minting submission tickets, and control cannot outlive observation.
        self.check_observation_live(now)?;
        if self.readiness != ViewReadiness::Ready {
            return Err(AuthorityError::ViewUnready);
        }
        let authorized_until = {
            let lease = self.lease.as_ref().ok_or(AuthorityError::NoLease)?;
            if lease.id != lease_id {
                return Err(AuthorityError::StaleLease);
            }
            if now > lease.authorized_until {
                return Err(AuthorityError::LeaseExpired);
            }
            lease.authorized_until
        };
        let uncapped = now
            .checked_add(self.policy.ticket_lifetime)
            .ok_or(AuthorityError::DeadlineOverflow)?;
        let expires_at = uncapped.min(authorized_until);
        let lease = self.lease.as_mut().expect("lease present above");
        lease.ticket = Some(Ticket {
            id: ticket_id,
            expires_at,
        });
        Ok(expires_at)
    }

    /// **The exact check the input agent runs immediately before every OS
    /// submission** (plan section 15.1). Verifies, against `now`: the phase,
    /// a `Ready` view, the current lease identity, the lease authorization
    /// deadline, and the referenced ticket's identity and expiry. Any failure
    /// is a typed refusal; nothing here mutates or renews — delayed traffic
    /// obtains no new lifetime at receipt.
    pub fn authorize_submission(
        &self,
        lease_id: InputLeaseId,
        ticket_id: InputTicketId,
        now: HostInstant,
    ) -> Result<(), AuthorityError> {
        if self.phase != Phase::Viewing {
            return Err(AuthorityError::InvalidState { phase: self.phase });
        }
        // Control authority is a strict subset of observation authority: an
        // action can never be submitted to a view the session is no longer
        // authorized to observe, even if the control lease was independently
        // renewed past the observation deadline (found in review by AzureBasin).
        self.check_observation_live(now)?;
        if self.readiness != ViewReadiness::Ready {
            return Err(AuthorityError::ViewUnready);
        }
        let lease = self.lease.ok_or(AuthorityError::NoLease)?;
        if lease.id != lease_id {
            return Err(AuthorityError::StaleLease);
        }
        if now > lease.authorized_until {
            return Err(AuthorityError::LeaseExpired);
        }
        let ticket = lease.ticket.ok_or(AuthorityError::TicketInvalid)?;
        if ticket.id != ticket_id {
            return Err(AuthorityError::TicketInvalid);
        }
        if now > ticket.expires_at {
            return Err(AuthorityError::TicketExpired);
        }
        Ok(())
    }

    /// Revokes the control lease synchronously at the authority decision
    /// point (plan section 15.2: local revoke has priority and never waits
    /// for a round trip). The held-key release and OS cleanup happen through
    /// the input path afterward; this call just makes the lease unusable.
    pub fn revoke_lease(&mut self) {
        self.lease = None;
        self.control_challenge = None;
    }

    /// True when the session currently holds a usable control lease: live
    /// observation, a `Ready` view, and a present, authorized lease. Control
    /// is a strict subset of observation. Read-only helper for the broker.
    #[must_use]
    pub fn has_live_control(&self, now: HostInstant) -> bool {
        self.check_observation_live(now).is_ok()
            && self.readiness == ViewReadiness::Ready
            && self.lease.is_some_and(|l| now <= l.authorized_until)
    }

    /// Applies a suspend/resume (or scheduler-stall) authority boundary
    /// (plan section 6.3). The deadline is checked against `now` — the
    /// post-resume instant — so a clock that jumped forward across a sleep
    /// retires any lapsed observation authorization, control lease, and
    /// pending challenges before any queued work is processed. Returns
    /// whether the control lease survived.
    pub fn apply_resume_boundary(&mut self, now: HostInstant) -> bool {
        if let Some(until) = self.observation_until {
            if now > until {
                self.drop_observation();
            }
        }
        if let Some(challenge) = self.observation_challenge {
            if now > challenge.deadline {
                self.observation_challenge = None;
            }
        }
        if let Some(challenge) = self.control_challenge {
            if now > challenge.deadline {
                self.control_challenge = None;
            }
        }
        let mut lease_survived = false;
        if let Some(lease) = self.lease {
            if now > lease.authorized_until {
                self.lease = None;
                self.control_challenge = None;
            } else if let Some(ticket) = lease.ticket {
                lease_survived = true;
                if now > ticket.expires_at {
                    // Lease lives, ticket does not: a new ticket is required
                    // before the next submission.
                    self.lease.as_mut().expect("present").ticket = None;
                }
            } else {
                lease_survived = true;
            }
        }
        lease_survived
    }

    /// Fences the session terminally: revoke control, drop observation, mark
    /// `Closed`. Ordering of the external cleanup (held-key releases, worker
    /// termination, closure publication) is the input/broker path's job; this
    /// records the authority decision (plan section 7.3).
    pub fn close(&mut self) {
        self.lease = None;
        self.observation_until = None;
        self.observation_challenge = None;
        self.control_challenge = None;
        self.readiness = ViewReadiness::Unready;
        self.phase = Phase::Closed;
    }

    /// Refuses the session terminally during negotiation.
    pub fn refuse(&mut self) {
        self.phase = Phase::Refused;
    }

    /// Retires observation authorization and demotes the phase so that
    /// reacquisition is a legal *new grant* (plan section 6.3: a short lease
    /// lapsing means re-authorizing, re-running approval where configured —
    /// not an inconsistent `Viewing` phase with no live observation). Any
    /// control lease is dropped too, since control cannot outlive observation.
    fn drop_observation(&mut self) {
        self.observation_until = None;
        self.readiness = ViewReadiness::Unready;
        self.lease = None;
        self.control_challenge = None;
        if matches!(self.phase, Phase::ViewOpening | Phase::Viewing) {
            self.phase = Phase::CapabilitiesChecked;
        }
    }

    fn check_observation_live(&self, now: HostInstant) -> Result<(), AuthorityError> {
        match self.observation_until {
            Some(until) if now <= until => Ok(()),
            Some(_) => Err(AuthorityError::ObservationExpired),
            None => Err(AuthorityError::ObservationExpired),
        }
    }

    fn deadline_from(&self, now: HostInstant) -> Result<HostInstant, AuthorityError> {
        now.checked_add(self.policy.authorization_lifetime)
            .ok_or(AuthorityError::DeadlineOverflow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (RemoteSessionId, InputLeaseId, InputTicketId) {
        (
            RemoteSessionId::from_raw(1),
            InputLeaseId::from_raw(2),
            InputTicketId::from_raw(3),
        )
    }

    fn at(micros: u64) -> HostInstant {
        HostInstant::from_micros(micros)
    }

    /// Drives a session to `Viewing` with a live lease and ticket at `t0`.
    fn viewing_with_control(t0: HostInstant) -> (SessionAuthority, InputLeaseId, InputTicketId) {
        let (s, lease, ticket) = ids();
        let mut a = SessionAuthority::new(s, AuthorityPolicy::plan_defaults());
        a.mark_capabilities_checked().unwrap();
        a.authorize_observation(t0).unwrap();
        a.mark_view_ready(t0).unwrap();
        a.grant_lease(lease, t0).unwrap();
        a.issue_input_ticket(lease, ticket, t0).unwrap();
        (a, lease, ticket)
    }

    #[test]
    fn first_frame_does_not_grant_control() {
        let (s, lease, _) = ids();
        let mut a = SessionAuthority::new(s, AuthorityPolicy::plan_defaults());
        a.mark_capabilities_checked().unwrap();
        a.authorize_observation(at(0)).unwrap();
        // View opening: observation authorized, but not yet ready.
        assert_eq!(
            a.grant_lease(lease, at(0)),
            Err(AuthorityError::InvalidState {
                phase: Phase::ViewOpening
            })
        );
        // Even once "viewing" is reached, control needs an explicit grant;
        // and a stale view blocks it.
        a.mark_view_ready(at(0)).unwrap();
        a.mark_view_stale();
        assert_eq!(
            a.grant_lease(lease, at(0)),
            Err(AuthorityError::ViewUnready)
        );
    }

    #[test]
    fn submission_check_holds_lease_ticket_expiry_and_readiness() {
        let t0 = at(0);
        let (a, lease, ticket) = viewing_with_control(t0);
        // Valid within the ticket lifetime (1s default).
        assert_eq!(a.authorize_submission(lease, ticket, at(500_000)), Ok(()));
        // Past the ticket deadline: refused, and nothing renewed it.
        assert_eq!(
            a.authorize_submission(lease, ticket, at(1_000_001)),
            Err(AuthorityError::TicketExpired)
        );
        // Wrong ticket id.
        assert_eq!(
            a.authorize_submission(lease, InputTicketId::from_raw(99), at(500_000)),
            Err(AuthorityError::TicketInvalid)
        );
        // Wrong lease id.
        assert_eq!(
            a.authorize_submission(InputLeaseId::from_raw(99), ticket, at(500_000)),
            Err(AuthorityError::StaleLease)
        );
    }

    #[test]
    fn stale_view_suspends_submission_without_dropping_the_lease() {
        let t0 = at(0);
        let (mut a, lease, ticket) = viewing_with_control(t0);
        a.mark_view_stale();
        assert_eq!(
            a.authorize_submission(lease, ticket, at(100_000)),
            Err(AuthorityError::ViewUnready)
        );
        // Recovering the view restores submission on the still-valid ticket.
        a.mark_view_ready(at(150_000)).unwrap();
        assert_eq!(a.authorize_submission(lease, ticket, at(200_000)), Ok(()));
    }

    #[test]
    fn delayed_challenge_response_after_expiry_does_not_resurrect() {
        let (s, _, _) = ids();
        let mut a = SessionAuthority::new(s, AuthorityPolicy::plan_defaults());
        a.mark_capabilities_checked().unwrap();
        a.authorize_observation(at(0)).unwrap();
        a.mark_view_ready(at(0)).unwrap();
        let deadline = a.issue_observation_challenge(0xabc, at(1_000_000));
        // A response after the deadlines is terminal, not a renewal. Because
        // observation authorization (3s) lapses at or before any challenge's
        // deadline, ObservationExpired is the fundamental refusal that fires —
        // reacquisition is a new grant either way (no resurrection).
        let late = HostInstant::from_micros(deadline.as_micros() + 1);
        assert_eq!(
            a.respond_observation_challenge(0xabc, late),
            Err(AuthorityError::ObservationExpired)
        );
        // A response with the wrong nonce, while observation is still live, is
        // refused as a challenge mismatch.
        a.issue_observation_challenge(0xdef, at(2_000_000));
        assert_eq!(
            a.respond_observation_challenge(0x111, at(2_100_000)),
            Err(AuthorityError::ChallengeMismatch)
        );
        // A timely, matching response renews.
        let renewed = a
            .respond_observation_challenge(0xdef, at(2_200_000))
            .unwrap();
        assert_eq!(renewed, at(2_200_000 + 3_000_000));
    }

    #[test]
    fn controller_slot_is_busy_until_revoked_then_handoff_serializes() {
        let t0 = at(0);
        let (mut a, first, _) = viewing_with_control(t0);
        let second = InputLeaseId::from_raw(42);
        // The slot is occupied by a live lease.
        assert_eq!(
            a.grant_lease(second, at(100_000)),
            Err(AuthorityError::ControllerBusy)
        );
        // Handoff is revoke-then-grant, decided in this one owner.
        a.revoke_lease();
        assert_eq!(a.grant_lease(second, at(200_000)), Ok(()));
        // The old lease's identity is dead; its submissions refuse.
        let ticket = InputTicketId::from_raw(7);
        a.issue_input_ticket(second, ticket, at(200_000)).unwrap();
        assert_eq!(
            a.authorize_submission(first, ticket, at(200_100)),
            Err(AuthorityError::StaleLease)
        );
    }

    #[test]
    fn resume_boundary_retires_a_lapsed_lease_across_a_clock_jump() {
        let t0 = at(0);
        let (mut a, lease, ticket) = viewing_with_control(t0);
        // Suspend, then resume well past the 3s authorization deadline. Both
        // observation and the lease lapse together.
        let resumed = at(10_000_000);
        let survived = a.apply_resume_boundary(resumed);
        assert!(!survived, "a lapsed lease must not survive resume");
        assert!(!a.has_live_control(resumed));
        assert_eq!(a.readiness(), ViewReadiness::Unready);
        // Observation lapsed, so the session demotes to CapabilitiesChecked:
        // reacquisition is a new grant, not an inconsistent Viewing phase.
        assert_eq!(a.phase(), Phase::CapabilitiesChecked);
        assert_eq!(
            a.authorize_submission(lease, ticket, resumed),
            Err(AuthorityError::InvalidState {
                phase: Phase::CapabilitiesChecked
            })
        );
        // Re-authorizing (the legal new-grant path) exposes the deeper truth:
        // the old lease was retired, so it is NoLease — never resurrected.
        a.authorize_observation(resumed).unwrap();
        a.mark_view_ready(resumed).unwrap();
        assert_eq!(
            a.authorize_submission(lease, ticket, resumed),
            Err(AuthorityError::NoLease)
        );
    }

    #[test]
    fn resume_boundary_keeps_a_live_lease_but_can_expire_only_its_ticket() {
        let t0 = at(0);
        let (mut a, lease, ticket) = viewing_with_control(t0);
        // Renew control authorization far into the future, but leave the
        // 1s ticket short. Resume between ticket expiry and lease expiry.
        a.issue_control_challenge(0x5, at(100_000)).unwrap();
        // Manually push the lease authorization out via a control-challenge
        // response so the lease outlives the ticket.
        // (issue at 100_000, respond at 150_000 -> authorized to 3_150_000)
        a.respond_control_challenge(lease, 0x5, at(150_000))
            .unwrap();
        let resumed = at(1_200_000); // past ticket (=1_000_000), before lease
        let survived = a.apply_resume_boundary(resumed);
        assert!(survived, "the lease itself is still authorized");
        // But the ticket is gone: a submission needs a fresh ticket.
        assert_eq!(
            a.authorize_submission(lease, ticket, resumed),
            Err(AuthorityError::TicketInvalid)
        );
        let ticket2 = InputTicketId::from_raw(77);
        a.issue_input_ticket(lease, ticket2, resumed).unwrap();
        assert_eq!(a.authorize_submission(lease, ticket2, resumed), Ok(()));
    }

    #[test]
    fn ticket_lifetime_is_clamped_to_the_lease_authorization() {
        // A lease authorized to 3s from t0; a ticket issued at 2.8s would
        // otherwise run to 3.8s but is clamped to the lease deadline.
        let t0 = at(0);
        let (mut a, lease, _) = viewing_with_control(t0);
        let ticket = InputTicketId::from_raw(9);
        let expires = a.issue_input_ticket(lease, ticket, at(2_800_000)).unwrap();
        assert_eq!(expires, at(3_000_000), "clamped to lease authorization");
    }

    #[test]
    fn close_and_revoke_are_terminal() {
        let t0 = at(0);
        let (mut a, lease, ticket) = viewing_with_control(t0);
        a.close();
        assert_eq!(a.phase(), Phase::Closed);
        assert_eq!(
            a.authorize_submission(lease, ticket, at(1)),
            Err(AuthorityError::InvalidState {
                phase: Phase::Closed
            })
        );
    }

    #[test]
    fn control_cannot_outlive_observation() {
        // Regression for AzureBasin's review finding: renewing the control
        // lease past the observation deadline must NOT authorize input, and
        // an observation challenge cannot resurrect lapsed observation.
        let t0 = at(0);
        let (mut a, lease, _) = viewing_with_control(t0);

        // Renew control far into the future via a control challenge, while
        // observation is deliberately never renewed (its deadline is 3s).
        a.issue_control_challenge(0x1, at(1_000_000)).unwrap();
        a.respond_control_challenge(lease, 0x1, at(1_500_000))
            .unwrap();
        // Now issue a ticket while observation is still live, then advance
        // past the observation deadline (3s) but within the renewed lease.
        let ticket = InputTicketId::from_raw(5);
        a.issue_input_ticket(lease, ticket, at(2_000_000)).unwrap();

        // At 3.5s: observation lapsed (deadline was 3s), lease still renewed.
        // Submission MUST be refused because observation is dead.
        assert_eq!(
            a.authorize_submission(lease, ticket, at(3_500_000)),
            Err(AuthorityError::ObservationExpired)
        );
        // Issuing another ticket is refused for the same reason.
        assert_eq!(
            a.issue_input_ticket(lease, InputTicketId::from_raw(6), at(3_500_000)),
            Err(AuthorityError::ObservationExpired)
        );
        // Renewing control is refused: control cannot outlive observation.
        a.issue_control_challenge(0x2, at(2_500_000)).unwrap();
        assert_eq!(
            a.respond_control_challenge(lease, 0x2, at(3_500_000)),
            Err(AuthorityError::ObservationExpired)
        );
        // has_live_control agrees.
        assert!(!a.has_live_control(at(3_500_000)));

        // And a stale observation challenge cannot resurrect it: issue while
        // live, respond after the observation deadline.
        let (mut b, _, _) = viewing_with_control(t0);
        b.issue_observation_challenge(0x9, at(2_000_000));
        assert_eq!(
            b.respond_observation_challenge(0x9, at(3_500_000)),
            Err(AuthorityError::ObservationExpired)
        );
    }

    #[test]
    fn expired_observation_blocks_view_ready_and_grant() {
        let (s, lease, _) = ids();
        let mut a = SessionAuthority::new(s, AuthorityPolicy::plan_defaults());
        a.mark_capabilities_checked().unwrap();
        a.authorize_observation(at(0)).unwrap();
        // 3s+ later without renewal: observation is expired.
        assert_eq!(
            a.mark_view_ready(at(3_000_001)),
            Err(AuthorityError::ObservationExpired)
        );
        assert_eq!(
            a.grant_lease(lease, at(3_000_001)),
            Err(AuthorityError::ObservationExpired)
        );
    }
}
