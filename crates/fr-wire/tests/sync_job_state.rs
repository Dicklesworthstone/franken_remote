//! Independent protocol fixtures for `0x0075 SyncJobState` (`PROTOCOL.md` §6).
use fr_core::{
    ids::{InputLeaseId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_wire::{
    WireError,
    files::{Context, Direction, Lane, Limits, Reason, Role},
    sync_job_state::{self, ConflictPolicy, RECORD_BYTES, SyncDirection, SyncJobState, SyncPhase},
};

fn limits() -> Limits {
    Limits::new(&ProtocolLimits::ABSOLUTE, 4096).unwrap()
}

fn context(sender: Role) -> Context {
    Context {
        session: RemoteSessionId::from_raw(u128::from_be_bytes([0x11; 16])),
        lease: InputLeaseId::from_raw(u128::from_be_bytes([0x22; 16])),
        handle: u128::from_be_bytes([0x33; 16]),
        channel: 42,
        sender,
        direction: Direction::ToHost,
        lane: Lane::Files,
    }
}

fn transferring() -> SyncJobState {
    SyncJobState {
        job_id: 9,
        sync_direction: SyncDirection::Bidirectional,
        conflict_policy: ConflictPolicy::KeepBoth,
        phase: SyncPhase::Transferring,
        reason: Reason::None,
        entries_total: 10,
        entries_completed: 4,
        conflicts_kept: 1,
        bytes_total: 1_048_576,
        bytes_completed: 262_144,
        manifest_revision: 3,
    }
}

fn fixture() -> Vec<u8> {
    // FRD0, v0, kind 0x0075, flags 0, res 0, payload 97 (0x61), binding 42 (0x2a), ext 0
    let hex = "
        46 52 44 30 00 00 00 75 00 00 00 00 00 00 00 61 00 00 00 2a 00 00 00 00
        11 11 11 11 11 11 11 11 11 11 11 11 11 11 11 11
        22 22 22 22 22 22 22 22 22 22 22 22 22 22 22 22
        33 33 33 33 33 33 33 33 33 33 33 33 33 33 33 33
        00 00 00 00 00 00 00 09
        03 01 04 00 00
        00 00 00 0a
        00 00 00 04
        00 00 00 01
        00 00 00 00 00 10 00 00
        00 00 00 00 00 04 00 00
        00 00 00 00 00 00 00 03
    ";
    hex.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

#[test]
fn golden_round_trip_and_truncation_matrix() {
    assert_eq!(RECORD_BYTES, 121);
    let expected = fixture();
    assert_eq!(expected.len(), RECORD_BYTES);

    let mut out = [0_u8; RECORD_BYTES];
    let written =
        sync_job_state::encode(&transferring(), context(Role::Host), limits(), &mut out).unwrap();
    assert_eq!(written, RECORD_BYTES);
    assert_eq!(out.as_slice(), expected.as_slice());

    let decoded = sync_job_state::decode(&expected, context(Role::Host), limits()).unwrap();
    assert_eq!(decoded, transferring());

    for cut in 0..expected.len() {
        assert!(
            sync_job_state::decode(&expected[..cut], context(Role::Host), limits()).is_err(),
            "must refuse truncation at {cut}"
        );
    }
}

#[test]
fn enforces_host_job_ownership_observer_exclusion_and_keep_both_policy() {
    let mut out = [0_u8; RECORD_BYTES];

    // Observer is always refused.
    assert_eq!(
        sync_job_state::encode(&transferring(), context(Role::Observer), limits(), &mut out),
        Err(WireError::WrongRole)
    );

    // Non-Files lane refused.
    let wrong_lane = Context {
        lane: Lane::Other,
        ..context(Role::Host)
    };
    assert_eq!(
        sync_job_state::encode(&transferring(), wrong_lane, limits(), &mut out),
        Err(WireError::WrongChannel)
    );

    // Controller CANNOT advertise a host-configured job (prevents arbitrary host path creation).
    let advertised = SyncJobState {
        phase: SyncPhase::Advertised,
        entries_completed: 0,
        conflicts_kept: 0,
        bytes_completed: 0,
        manifest_revision: 0,
        ..transferring()
    };
    assert!(sync_job_state::encode(&advertised, context(Role::Host), limits(), &mut out).is_ok());
    assert_eq!(
        sync_job_state::encode(&advertised, context(Role::Controller), limits(), &mut out),
        Err(WireError::WrongRole)
    );

    // Only controller can send Accepted.
    let accepted = SyncJobState {
        phase: SyncPhase::Accepted,
        ..advertised
    };
    assert!(
        sync_job_state::encode(&accepted, context(Role::Controller), limits(), &mut out).is_ok()
    );
    assert_eq!(
        sync_job_state::encode(&accepted, context(Role::Host), limits(), &mut out),
        Err(WireError::WrongRole)
    );

    // Wire conflict_policy != KeepBoth (e.g. 0 or 2 = silent overwrite) must refuse.
    for bad_policy in [0_u8, 2, 255] {
        let mut bad = fixture();
        bad[81] = bad_policy;
        assert_eq!(
            sync_job_state::decode(&bad, context(Role::Host), limits()),
            Err(WireError::InvalidValue)
        );
    }

    // Completed entries/bytes exceeding totals refused.
    assert_eq!(
        sync_job_state::encode(
            &SyncJobState {
                entries_completed: 11,
                ..transferring()
            },
            context(Role::Host),
            limits(),
            &mut out
        ),
        Err(WireError::InvalidValue)
    );
    // Settled requires completed == total.
    assert_eq!(
        sync_job_state::encode(
            &SyncJobState {
                phase: SyncPhase::Settled,
                ..transferring()
            },
            context(Role::Host),
            limits(),
            &mut out
        ),
        Err(WireError::InvalidValue)
    );
    // Failed requires nonzero reason.
    assert_eq!(
        sync_job_state::encode(
            &SyncJobState {
                phase: SyncPhase::Failed,
                reason: Reason::None,
                ..transferring()
            },
            context(Role::Host),
            limits(),
            &mut out
        ),
        Err(WireError::InvalidValue)
    );
}
