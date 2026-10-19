//! File receipt keeps original control authority without borrowing clipboard's
//! publication readiness or keeping a replaced input owner alive.
use super::*;
use fr_client::files::{ControllerAuthority, Error as FileError};
use fr_core::{
    ids::{InputLeaseId, OsSessionId},
    input_submission::Refusal,
};
use fr_wire::negotiation::ControlBinding;

fn receive(input: &mut InputClient, elapsed: u64) -> Result<ControllerAuthority, FileError> {
    input.file_receive_authority(grant().request.parent, 78, grant().lease, client(elapsed))
}

#[test]
fn file_receive_handoff_requires_an_accepted_mapped_grant_and_exact_one_use_lane() {
    let g = grant();
    let mut fabricated = InputClient::new(
        g.credentials(),
        9,
        g.request.target.bounds,
        g.request.target.capabilities,
        L,
        policy(),
        client(0),
    )
    .unwrap();
    ready(&mut fabricated, 0, 0);
    assert_eq!(
        receive(&mut fabricated, 0).unwrap_err(),
        FileError::NotGranted
    );
    let mut input = accepted();
    assert_eq!(receive(&mut input, 0).unwrap_err(), FileError::NotReady);
    ready(&mut input, 0, 0);
    for channel in [0, 7, 8, 9] {
        assert_eq!(
            input
                .file_receive_authority(g.request.parent, channel, g.lease, client(0))
                .unwrap_err(),
            FileError::WrongChannel
        );
    }
    let foreign = ControlBinding {
        os_session: OsSessionId::from_raw(99),
        ..g.request.parent
    };
    assert_eq!(
        input
            .file_receive_authority(foreign, 78, g.lease, client(0))
            .unwrap_err(),
        FileError::WrongChannel
    );
    assert_eq!(
        input
            .file_receive_authority(g.request.parent, 78, InputLeaseId::from_raw(99), client(0))
            .unwrap_err(),
        FileError::WrongChannel
    );
    let authority = receive(&mut input, 0).unwrap();
    assert_eq!(authority.parent(), g.request.parent);
    assert_eq!(authority.channel(), 78);
    assert_eq!(authority.lease(), g.lease);
    drop(authority);
    assert_eq!(
        receive(&mut input, 1).unwrap_err(),
        FileError::AlreadyAttached
    );
}

#[test]
fn file_clock_uses_original_host_projection_and_survives_owner_updates() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let authority = receive(&mut input, 0).unwrap();
    let at = authority.sample(client(10)).unwrap();
    assert!(at.as_micros() >= H + 100 && at.as_micros() < H + 1000);
    assert!(at.as_micros() < C);
    input.tick(client(20)).unwrap();
    authority.deadline(at).unwrap();
    assert!(authority.sample(client(9)).is_err());
    assert!(authority.sample(client(21)).is_err());
    input.tick(client(22)).unwrap();
    assert!(input.stopped().is_none());
}

#[test]
fn file_authority_expires_during_suspension_without_reviving_on_a_later_ticket() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let authority = receive(&mut input, 0).unwrap();
    input.tick(client(1_500_001)).unwrap();
    assert!(input.suspended_since().is_some());
    authority.sample(client(1_500_002)).unwrap();
    assert!(authority.sample(client(3_000_001)).is_err());
    let newer = ticket(1, H + 3_000_001, H + 3_900_001);
    input
        .accept_ticket(&newer, correlation(3_000_001), client(3_000_001))
        .unwrap();
    assert!(authority.sample(client(3_000_002)).is_err());
    assert_eq!(
        receive(&mut input, 3_000_002).unwrap_err(),
        FileError::NotGranted
    );
}

#[test]
fn file_revoke_is_independent_of_clipboard_and_input() {
    let (mut input, mut clipboard) = attached();
    let authority = receive(&mut input, 0).unwrap();
    authority.revoke();
    assert_eq!(authority.sample(client(1)), Err(Refusal::Revoked));
    input.tick(client(2)).unwrap();
    clipboard
        .offer(1, "still approved", None, client(2))
        .unwrap();
    assert!(input.stopped().is_none());

    let (mut input, mut clipboard) = attached();
    let authority = receive(&mut input, 0).unwrap();
    clipboard.close();
    authority.sample(client(1)).unwrap();
    input.tick(client(2)).unwrap();
    assert!(input.stopped().is_none());
}

#[test]
fn file_handoff_cannot_outlive_drop_stop_or_replacement_of_original_controller() {
    for stop in [false, true] {
        let mut input = accepted();
        ready(&mut input, 0, 0);
        let authority = receive(&mut input, 0).unwrap();
        if stop {
            input.stop(StopReason::FocusLost);
        } else {
            drop(input);
        }
        let mut replacement = accepted();
        ready(&mut replacement, 0, 0);
        receive(&mut replacement, 0)
            .unwrap()
            .sample(client(1))
            .unwrap();
        assert_eq!(authority.sample(client(1)), Err(Refusal::Revoked));
    }
}
