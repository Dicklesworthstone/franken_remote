//! Independent protocol fixtures for `0x0081 ReceiverPressure` (`PROTOCOL.md` §5, §6).
use fr_core::{
    ids::{HostBootId, OsSessionId, RemoteSessionId},
    limits::ProtocolLimits,
};
use fr_wire::{
    WireError,
    input::{InputDelivery as Delivery, InputDirection as Direction},
    negotiation::ControlBinding,
    receiver_pressure::{self, ChannelRole, QueuePressure, RECORD_BYTES, ReceiverPressure},
};

const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;

fn binding() -> ControlBinding {
    ControlBinding {
        id: 0x0102_0304,
        host_boot: HostBootId::from_raw(u128::from_be_bytes([0x11; 16])),
        os_session: OsSessionId::from_raw(u128::from_be_bytes([0x22; 16])),
        remote_session: RemoteSessionId::from_raw(u128::from_be_bytes([0x33; 16])),
    }
}

fn report() -> ReceiverPressure {
    ReceiverPressure {
        channel_binding: 0x0506_0708,
        channel_role: ChannelRole::Video,
        credit_epoch: 7,
        released_bytes: 65_536,
        released_frames: 16,
        pressure: QueuePressure::Moderate,
        queued_bytes: 16_384,
        queued_items: 4,
    }
}

fn fixture() -> Vec<u8> {
    // FRD0, v0, kind 0x0081, flags 0, res 0, payload 91 (0x5b), binding 0x01020304, ext 0
    let hex = "
        46 52 44 30 00 00 00 81 00 00 00 00 00 00 00 5b 01 02 03 04 00 00 00 00
        11 11 11 11 11 11 11 11 11 11 11 11 11 11 11 11
        22 22 22 22 22 22 22 22 22 22 22 22 22 22 22 22
        33 33 33 33 33 33 33 33 33 33 33 33 33 33 33 33
        05 06 07 08
        01 01
        00 00 00 00 00 00 00 07
        00 00 00 00 00 01 00 00
        00 00 00 00 00 00 00 10
        01
        00 00 00 00 00 00 40 00
        00 00 00 04
    ";
    hex.split_whitespace()
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect()
}

#[test]
fn golden_round_trip_and_truncation_matrix() {
    assert_eq!(RECORD_BYTES, 115);
    let expected = fixture();
    assert_eq!(expected.len(), RECORD_BYTES);

    let mut out = [0_u8; RECORD_BYTES];
    let written = receiver_pressure::encode(
        &report(),
        binding(),
        &LIMITS,
        &mut out,
        Direction::ViewerToHost,
        Delivery::Reliable,
    )
    .unwrap();
    assert_eq!(written, RECORD_BYTES);
    assert_eq!(out.as_slice(), expected.as_slice());

    let decoded = receiver_pressure::decode(
        &expected,
        binding(),
        &LIMITS,
        Direction::ViewerToHost,
        Delivery::Reliable,
    )
    .unwrap();
    assert_eq!(decoded, report());

    for cut in 0..expected.len() {
        assert!(
            receiver_pressure::decode(
                &expected[..cut],
                binding(),
                &LIMITS,
                Direction::ViewerToHost,
                Delivery::Reliable,
            )
            .is_err(),
            "must refuse truncation at {cut}"
        );
    }
}

#[test]
fn direction_role_epoch_and_budget_invariants_enforced() {
    let mut out = [0_u8; RECORD_BYTES];
    // Video receiver is Viewer -> HostToViewer report must refuse WrongRole.
    assert_eq!(
        receiver_pressure::encode(
            &report(),
            binding(),
            &LIMITS,
            &mut out,
            Direction::HostToViewer,
            Delivery::Reliable,
        ),
        Err(WireError::WrongRole)
    );
    // Files (controller->host) receiver is Host -> HostToViewer succeeds, ViewerToHost refuses.
    let files_report = ReceiverPressure {
        channel_role: ChannelRole::Files,
        ..report()
    };
    assert!(
        receiver_pressure::encode(
            &files_report,
            binding(),
            &LIMITS,
            &mut out,
            Direction::HostToViewer,
            Delivery::Reliable,
        )
        .is_ok()
    );
    assert_eq!(
        receiver_pressure::encode(
            &files_report,
            binding(),
            &LIMITS,
            &mut out,
            Direction::ViewerToHost,
            Delivery::Reliable,
        ),
        Err(WireError::WrongRole)
    );

    // Zero credit epoch refused.
    assert_eq!(
        receiver_pressure::encode(
            &ReceiverPressure {
                credit_epoch: 0,
                ..report()
            },
            binding(),
            &LIMITS,
            &mut out,
            Direction::ViewerToHost,
            Delivery::Reliable,
        ),
        Err(WireError::InvalidBinding)
    );

    // Asymmetric released bytes vs frames refused.
    assert_eq!(
        receiver_pressure::encode(
            &ReceiverPressure {
                released_bytes: 1024,
                released_frames: 0,
                ..report()
            },
            binding(),
            &LIMITS,
            &mut out,
            Direction::ViewerToHost,
            Delivery::Reliable,
        ),
        Err(WireError::InvalidValue)
    );

    // Exceeding compressed-bytes limit refused before admission.
    assert_eq!(
        receiver_pressure::encode(
            &ReceiverPressure {
                queued_bytes: LIMITS.per_viewer_compressed_bytes() + 1,
                ..report()
            },
            binding(),
            &LIMITS,
            &mut out,
            Direction::ViewerToHost,
            Delivery::Reliable,
        ),
        Err(WireError::ResourceLimit)
    );

    // Datagram delivery refused.
    assert_eq!(
        receiver_pressure::encode(
            &report(),
            binding(),
            &LIMITS,
            &mut out,
            Direction::ViewerToHost,
            Delivery::Datagram,
        ),
        Err(WireError::WrongChannel)
    );
}
