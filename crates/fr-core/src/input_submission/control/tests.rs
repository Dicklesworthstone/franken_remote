//! Clock-injected tests of the real authority and native-lease owners. No OS
//! submission or network qualification is implied by these policy fixtures.
use super::*;
use crate::{
    authority::AuthorityPolicy,
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputTicketId, RecoveryGeneration,
        ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::Capabilities,
};

struct Fixture {
    authority: Arc<Mutex<SessionAuthority>>,
    input: InputSession,
    renewal: ControlLease,
    credentials: InputCredentials,
}
fn at(us: u64) -> HostInstant {
    HostInstant::from_micros(us)
}
fn fixture(view_until: u64) -> Fixture {
    let credentials = InputCredentials {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        ticket: InputTicketId::from_raw(3),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    };
    let mut authority =
        SessionAuthority::new(credentials.session, AuthorityPolicy::plan_defaults());
    authority.mark_capabilities_checked().unwrap();
    authority.authorize_observation(at(0)).unwrap();
    authority
        .mark_view_ready_until(at(view_until), at(0))
        .unwrap();
    authority.grant_lease(credentials.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
        .unwrap();
    let authority = Arc::new(Mutex::new(authority));
    let mut input = InputSession::from_shared_authority(
        authority.clone(),
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 16, 16).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap();
    let renewal = input.take_control_lease().unwrap();
    Fixture {
        authority,
        input,
        renewal,
        credentials,
    }
}

#[test]
fn a_nearly_expired_picture_does_not_shorten_the_renewal_budget() {
    let mut f = fixture(250_000);
    for now in [0, 100_000, 249_999] {
        assert_eq!(f.renewal.deadline(at(now)), Ok(at(3_000_000)));
        // The native input gate still has the original, short view deadline.
        assert_eq!(f.input.monitor().deadline(at(now)), Ok(at(250_000)));
    }
    assert_eq!(f.renewal.deadline(at(250_000)), Ok(at(3_000_000)));
    assert!(matches!(
        f.input.monitor().status(at(250_000)),
        Ok(ControlStatus::Suspended {
            until,
            released: false,
        }) if until == at(3_000_000)
    ));
    assert!(!f.renewal.stopped());
    assert!(f.input.take_control_lease().is_none());
}

#[test]
fn a_response_after_view_expiry_keeps_its_issue_time_deadline_and_input_fenced() {
    let mut f = fixture(250_000);
    let issued_until = f.renewal.challenge(11, at(100_000)).unwrap();
    assert_eq!(issued_until, at(3_100_000));
    assert_eq!(f.renewal.respond(11, at(600_000)), Ok(issued_until));
    assert_eq!(f.renewal.lease(), f.credentials.lease);
    assert_eq!(f.renewal.session(), f.credentials.session);
    // Observation still expires at 3s. The response neither renews it nor
    // establishes a fresh view, a usable ticket, or completed native release.
    assert_eq!(f.renewal.deadline(at(600_001)), Ok(at(3_000_000)));
    assert!(matches!(
        f.input.monitor().status(at(600_001)),
        Ok(ControlStatus::Suspended {
            released: false,
            ..
        })
    ));
    assert!(
        f.authority
            .lock()
            .unwrap()
            .authorize_submission(f.credentials.lease, f.credentials.ticket, at(600_001))
            .is_err()
    );
    assert!(!f.renewal.stopped());
}

#[test]
fn renewal_still_ends_at_the_earlier_lease_or_observation_deadline() {
    for renew_control in [false, true] {
        let mut f = fixture(2_000_000);
        if renew_control {
            f.renewal.challenge(12, at(1_000_000)).unwrap();
            f.renewal.respond(12, at(1_100_000)).unwrap();
        } else {
            let mut a = f.authority.lock().unwrap();
            a.issue_observation_challenge(12, at(1_000_000)).unwrap();
            a.respond_observation_challenge(12, at(1_100_000)).unwrap();
        }
        assert_eq!(f.renewal.deadline(at(1_200_000)), Ok(at(3_000_000)));
        assert_eq!(f.input.monitor().deadline(at(1_200_000)), Ok(at(2_000_000)));
        assert_eq!(f.renewal.deadline(at(2_999_999)), Ok(at(3_000_000)));
        assert!(f.renewal.deadline(at(3_000_000)).is_err());
        assert!(f.renewal.stopped());
        // Neither an old response nor later deadline sampling revives it.
        assert!(f.renewal.respond(12, at(3_000_001)).is_err());
    }
}

#[test]
fn continuing_renewals_cannot_extend_the_original_suspension_limit() {
    let mut f = fixture(250_000);
    // Establish the lapse at the original view deadline, before renewals.
    assert_eq!(f.renewal.deadline(at(250_000)), Ok(at(3_000_000)));
    for second in 1..=10_u64 {
        let now = at(second * 1_000_000);
        let nonce = u128::from(second);
        {
            let mut a = f.authority.lock().unwrap();
            a.issue_observation_challenge(nonce, now).unwrap();
            a.respond_observation_challenge(nonce, now).unwrap();
        }
        f.renewal.challenge(nonce, now).unwrap();
        f.renewal.respond(nonce, now).unwrap();
    }
    assert_eq!(f.renewal.deadline(at(10_000_001)), Ok(at(10_250_000)));
    assert!(f.renewal.deadline(at(10_250_000)).is_err());
    assert!(f.renewal.stopped());
}

#[test]
fn local_revocation_still_fences_renewal_without_waiting_for_a_picture_or_peer() {
    let mut f = fixture(250_000);
    f.renewal.challenge(17, at(1)).unwrap();
    f.input.revoke();
    assert!(f.renewal.deadline(at(2)).is_err());
    assert!(f.renewal.respond(17, at(2)).is_err());
    assert!(f.renewal.stopped());
}
