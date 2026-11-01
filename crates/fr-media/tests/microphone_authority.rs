//! Pure original-input ownership evidence. No native device, approval UI or
//! network admission is implied by this policy fixture.
use fr_core::{
    authority::{AuthorityPolicy, SessionAuthority},
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputBounds, InputCredentials, InputView},
    input_submission::{Capabilities, InputSession},
    time::HostInstant,
};
use fr_media::audio::authority::{HostController, Scope};
use std::{sync::mpsc, thread};

fn at(micros: u64) -> HostInstant {
    HostInstant::from_micros(micros)
}
fn input() -> InputSession {
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
    authority.mark_view_ready(at(0)).unwrap();
    authority.grant_lease(credentials.lease, at(0)).unwrap();
    authority
        .issue_input_ticket(credentials.lease, credentials.ticket, at(0))
        .unwrap();
    InputSession::new(
        authority,
        credentials,
        InputBounds::new(DesktopPoint { x: 0, y: 0 }, 320, 240).unwrap(),
        Capabilities::default(),
        at(0),
    )
    .unwrap()
}
fn renew(input: &mut InputSession, now: HostInstant) {
    input.issue_observation_challenge(1, now).unwrap();
    input.renew_observation(1, now).unwrap();
    input.issue_control_challenge(2, now).unwrap();
    input.renew_control(2, now).unwrap();
}

#[test]
fn handoff_remains_bound_after_moving_input_to_its_worker() {
    let original = input();
    let controller = HostController::from_input(&original);
    assert_eq!(
        controller.scope(),
        Scope {
            session: RemoteSessionId::from_raw(1),
            lease: InputLeaseId::from_raw(2),
        }
    );
    let (finish, receive) = mpsc::sync_channel(0);
    let worker = thread::spawn(move || {
        receive.recv().unwrap();
        drop(original);
    });
    assert_eq!(controller.deadline(at(1)), Ok(at(3_000_000)));
    finish.send(()).unwrap();
    worker.join().unwrap();
    assert!(controller.deadline(at(2)).is_err());
}

#[test]
fn equal_ids_cannot_revive_a_dropped_owner_or_an_escaped_clone() {
    let original = input();
    let old = HostController::from_input(&original);
    let escaped = old.clone();
    drop(original);
    let replacement = input();
    let current = HostController::from_input(&replacement);
    assert_eq!(old.scope(), current.scope());
    assert!(old.deadline(at(1)).is_err());
    assert!(escaped.deadline(at(1)).is_err());
    assert_eq!(current.deadline(at(1)), Ok(at(3_000_000)));
    drop(old);
    drop(escaped);
    assert!(current.deadline(at(2)).is_ok());
}

#[test]
fn only_original_control_renewal_changes_future_admission() {
    let mut original = input();
    let controller = HostController::from_input(&original);
    let original_cap = controller.deadline(at(0)).unwrap();
    // Read-only conservative samples must not advance the input owner's
    // serialized mutation clock and reject an earlier in-flight renewal.
    assert_eq!(controller.deadline(at(2_000_000)), Ok(original_cap));
    renew(&mut original, at(1_000_000));
    assert_eq!(controller.deadline(at(3_000_000)), Ok(at(4_000_000)));
    assert_eq!(original_cap, at(3_000_000));
    original.revoke();
    assert!(controller.deadline(at(3_000_001)).is_err());
    assert!(original.issue_control_challenge(3, at(3_000_001)).is_err());
}

#[test]
fn expiry_is_inclusive_and_projection_drop_cannot_revoke_input() {
    let mut original = input();
    let controller = HostController::from_input(&original);
    drop(controller.clone());
    assert!(original.monitor().deadline(at(2_999_999)).is_ok());
    assert!(controller.deadline(at(3_000_000)).is_err());
    assert!(original.monitor().is_revoked());
    assert!(original.issue_control_challenge(1, at(3_000_000)).is_err());
    assert!(controller.deadline(at(2_999_999)).is_err());
}
