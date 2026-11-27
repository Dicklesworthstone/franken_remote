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

#[test]
fn publication_clock_keeps_the_original_projection_cursor_and_terminal_fences() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let authority = receive(&mut input, 0).unwrap();
    assert!(!authority.is_revoked());
    let before = authority.sample(client(10)).unwrap();
    input.tick(client(20)).unwrap();
    let final_sample = authority.try_sample(client(11)).unwrap();
    assert!(final_sample >= before);
    assert!(final_sample.as_micros() < C);
    authority.deadline(final_sample).unwrap();
    // Both sample methods share one ordered consumer; trying to reuse an older
    // timestamp cannot obtain a different publication clock.
    assert!(authority.try_sample(client(9)).is_err());
    assert!(authority.is_revoked());
    assert!(authority.sample(client(21)).is_err());
    input.tick(client(22)).unwrap();
    assert!(input.stopped().is_none());
}

#[test]
fn publication_clock_never_extends_suspended_or_dropped_file_authority() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let authority = receive(&mut input, 0).unwrap();
    input.tick(client(1_500_001)).unwrap();
    assert!(input.suspended_since().is_some());
    authority.try_sample(client(1_500_002)).unwrap();
    assert!(authority.try_sample(client(3_000_001)).is_err());
    assert!(authority.is_revoked());

    let mut input = accepted();
    ready(&mut input, 0, 0);
    let authority = receive(&mut input, 0).unwrap();
    drop(input);
    assert!(authority.is_revoked());
    assert_eq!(authority.try_sample(client(1)), Err(Refusal::Revoked));
}

fn source(input: &mut InputClient, elapsed: u64) -> Result<ControllerAuthority, FileError> {
    input.file_send_authority(grant().request.parent, 79, grant().lease, client(elapsed))
}

#[test]
fn source_handoff_requires_the_original_mapped_grant_and_one_exact_attachment() {
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
        source(&mut fabricated, 0).unwrap_err(),
        FileError::NotGranted
    );
    let mut input = accepted();
    assert_eq!(source(&mut input, 0).unwrap_err(), FileError::NotReady);
    ready(&mut input, 0, 0);
    for channel in [0, 7, 8, 9] {
        assert_eq!(
            input
                .file_send_authority(g.request.parent, channel, g.lease, client(0))
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
            .file_send_authority(foreign, 79, g.lease, client(0))
            .unwrap_err(),
        FileError::WrongChannel
    );
    assert_eq!(
        input
            .file_send_authority(g.request.parent, 79, InputLeaseId::from_raw(99), client(0))
            .unwrap_err(),
        FileError::WrongChannel
    );
    let authority = source(&mut input, 0).unwrap();
    assert_eq!(authority.parent(), g.request.parent);
    assert_eq!(authority.channel(), 79);
    assert_eq!(authority.lease(), g.lease);
    drop(authority);
    assert_eq!(
        source(&mut input, 1).unwrap_err(),
        FileError::AlreadyAttached
    );
}

#[test]
fn duplex_file_authority_cancellation_is_independent_in_both_attachment_orders() {
    for source_first in [false, true] {
        for revoke_source in [false, true] {
            let (mut input, mut clipboard) = attached();
            let (upload, download) = if source_first {
                (
                    source(&mut input, 0).unwrap(),
                    receive(&mut input, 0).unwrap(),
                )
            } else {
                let download = receive(&mut input, 0).unwrap();
                (source(&mut input, 0).unwrap(), download)
            };
            let (cancelled, live) = if revoke_source {
                (&upload, &download)
            } else {
                (&download, &upload)
            };
            cancelled.revoke();
            assert_eq!(cancelled.check(client(1)), Err(Refusal::Revoked));
            live.check(client(1)).unwrap();
            input.tick(client(2)).unwrap();
            live.check(client(3)).unwrap();
            clipboard
                .offer(1, "independent clipboard", None, client(3))
                .unwrap();
            assert!(input.stopped().is_none());
            let failed = if revoke_source {
                source(&mut input, 4)
            } else {
                receive(&mut input, 4)
            };
            assert_eq!(failed.unwrap_err(), FileError::NotGranted);
        }
    }
}

#[test]
fn both_file_directions_follow_valid_tickets_but_expired_sources_never_revive() {
    let mut input = accepted();
    ready(&mut input, 0, 0);
    let upload = source(&mut input, 0).unwrap();
    let download = receive(&mut input, 0).unwrap();
    ready(&mut input, 1, 2_000_000);
    input
        .accept_ticket(
            &ticket(1, H + 2_000_000, H + 3_500_000),
            correlation(2_000_000),
            client(2_000_000),
        )
        .unwrap();
    for authority in [&upload, &download] {
        // This is later than the initial lease proof: only the original
        // validated ticket update can keep either worker authorized.
        authority.check(client(3_000_100)).unwrap();
        assert!(authority.check(client(3_900_001)).is_err());
        assert!(authority.is_revoked());
    }
    input
        .accept_ticket(
            &ticket(2, H + 4_000_000, H + 5_000_000),
            correlation(4_000_000),
            client(4_000_000),
        )
        .unwrap();
    assert!(upload.check(client(4_000_001)).is_err());
    assert!(download.check(client(4_000_001)).is_err());
    assert_eq!(
        source(&mut input, 4_000_002).unwrap_err(),
        FileError::NotGranted
    );
    assert_eq!(
        receive(&mut input, 4_000_002).unwrap_err(),
        FileError::NotGranted
    );
}

#[test]
fn original_source_stops_without_owner_ticks_on_drop_focus_loss_or_expiry() {
    for failure in 0..3 {
        let mut input = accepted();
        ready(&mut input, 0, 0);
        let upload = source(&mut input, 0).unwrap();
        let download = receive(&mut input, 0).unwrap();
        let at = match failure {
            0 => {
                input.stop(StopReason::FocusLost);
                client(1)
            }
            1 => {
                drop(input);
                client(1)
            }
            _ => client(3_000_001),
        };
        let mut replacement = accepted();
        ready(&mut replacement, 0, 0);
        source(&mut replacement, 0)
            .unwrap()
            .check(client(1))
            .unwrap();
        for authority in [&upload, &download] {
            assert!(authority.check(at).is_err());
            assert!(authority.is_revoked());
        }
    }
}
