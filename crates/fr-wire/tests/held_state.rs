use fr_core::{
    held_state::{HeldState, HeldStateRequest},
    ids::{InputLeaseId, RemoteSessionId},
    input::{PhysicalKey, PointerButton},
    limits::ProtocolLimits,
};
use fr_wire::{
    WireError,
    held_state::{HELD_STATE_BYTES, decode, encode},
    input::{InputDelivery as D, InputDirection as R},
};
fn request() -> HeldStateRequest {
    let mut held = HeldState::empty();
    held.set_key(PhysicalKey::new(4).unwrap(), true);
    held.set_key(PhysicalKey::new(0xe1).unwrap(), true);
    held.set_button(PointerButton::Primary, true);
    HeldStateRequest {
        session: RemoteSessionId::from_raw(1),
        lease: InputLeaseId::from_raw(2),
        sequence: 3,
        next_action: 4,
        held,
    }
}
fn bytes() -> [u8; HELD_STATE_BYTES] {
    let mut result = [0; HELD_STATE_BYTES];
    encode(
        request(),
        &mut result,
        &ProtocolLimits::ABSOLUTE,
        9,
        R::ViewerToHost,
        D::Reliable,
    )
    .unwrap();
    result
}
#[test]
fn exact_independently_constructed_wire_bytes_and_every_truncation() {
    let data = bytes();
    let mut golden = [0; HELD_STATE_BYTES];
    golden[..24].copy_from_slice(&[
        b'F', b'R', b'D', b'0', 0, 0, 0, 0x46, 0, 0, 0, 0, 0, 0, 0, 81, 0, 0, 0, 9, 0, 0, 0, 0,
    ]);
    golden[39] = 1;
    golden[55] = 2;
    golden[63] = 3;
    golden[71] = 4;
    golden[72] = 16;
    golden[100] = 2;
    golden[104] = 1;
    assert_eq!(data, golden);
    assert_eq!(
        decode(
            &golden,
            &ProtocolLimits::ABSOLUTE,
            9,
            R::ViewerToHost,
            D::Reliable
        )
        .unwrap(),
        request()
    );
    for end in 0..data.len() {
        assert!(
            decode(
                &data[..end],
                &ProtocolLimits::ABSOLUTE,
                9,
                R::ViewerToHost,
                D::Reliable
            )
            .is_err(),
            "truncation {end}"
        );
    }
}
#[test]
fn refuses_reserved_bits_zero_ids_wrong_binding_role_and_datagrams() {
    for (offset, value) in [(72, 1), (104, 0x80), (39, 0), (55, 0)] {
        let mut data = bytes();
        data[offset] = value;
        assert!(
            decode(
                &data,
                &ProtocolLimits::ABSOLUTE,
                9,
                R::ViewerToHost,
                D::Reliable
            )
            .is_err()
        );
    }
    let data = bytes();
    assert_eq!(
        decode(
            &data,
            &ProtocolLimits::ABSOLUTE,
            9,
            R::HostToViewer,
            D::Reliable
        ),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        decode(
            &data,
            &ProtocolLimits::ABSOLUTE,
            9,
            R::ViewerToHost,
            D::Datagram
        ),
        Err(WireError::WrongChannel)
    );
    assert!(
        decode(
            &data,
            &ProtocolLimits::ABSOLUTE,
            8,
            R::ViewerToHost,
            D::Reliable
        )
        .is_err()
    );
    let mut out = [0; HELD_STATE_BYTES];
    assert!(
        encode(
            request(),
            &mut out,
            &ProtocolLimits::ABSOLUTE,
            0,
            R::ViewerToHost,
            D::Reliable
        )
        .is_err()
    );
}
#[test]
fn validates_complete_bitmap_and_redacts_state_and_binding() {
    for usage in 0_u16..256 {
        let mut bits = [0; 32];
        bits[usize::from(usage / 8)] = 1 << (usage % 8);
        assert_eq!(
            HeldState::from_bits(bits, 0).is_some(),
            PhysicalKey::new(usage).is_some()
        );
    }
    assert_eq!(format!("{:?}", request()), "HeldStateRequest([redacted])");
    assert_eq!(format!("{:?}", request().held), "HeldState([redacted])");
}
