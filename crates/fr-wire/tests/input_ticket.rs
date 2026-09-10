use fr_core::{
    ids::*,
    input::{InputCredentials, InputView},
    limits::{LimitOverrides, ProtocolLimits},
};
use fr_wire::{
    WireError,
    input::{InputDelivery as Delivery, InputDirection as Direction},
    input_ticket::{self, INPUT_TICKET_BYTES, Ticket},
};
const LIMITS: ProtocolLimits = ProtocolLimits::ABSOLUTE;
fn ticket() -> Ticket {
    Ticket {
        credentials: InputCredentials {
            session: RemoteSessionId::from_raw(u128::from_be_bytes([0x11; 16])),
            lease: InputLeaseId::from_raw(u128::from_be_bytes([0x22; 16])),
            ticket: InputTicketId::from_raw(u128::from_be_bytes([0x33; 16])),
            view: InputView {
                geometry: DisplayGeometryGeneration::from_raw(1),
                viewport: ViewportMappingGeneration::from_raw(2),
                configuration: CodecConfigurationGeneration::from_raw(3),
                recovery: RecoveryGeneration::from_raw(4),
            },
        },
        sequence: 5,
        issued_at_us: 6,
        expires_at_us: 1_000_006,
    }
}
fn encode(t: Ticket) -> Vec<u8> {
    let mut out = vec![0; INPUT_TICKET_BYTES];
    assert_eq!(
        input_ticket::encode(
            t,
            &mut out,
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable
        ),
        Ok(INPUT_TICKET_BYTES)
    );
    out
}
fn decode(b: &[u8]) -> Result<Ticket, WireError> {
    input_ticket::decode(b, &LIMITS, 7, Direction::HostToViewer, Delivery::Reliable)
}
#[test]
fn independent_exact_bytes_and_every_truncation() {
    let mut b = vec![
        0x46, 0x52, 0x44, 0x30, 0, 0, 0, 0x17, 0, 0, 0, 0, 0, 0, 0, 104, 0, 0, 0, 7, 0, 0, 0, 0,
    ];
    b.extend_from_slice(&[0x11; 16]);
    b.extend_from_slice(&[0x22; 16]);
    b.extend_from_slice(&[0x33; 16]);
    for value in [1_u64, 2, 3, 4, 5, 6, 1_000_006] {
        b.extend_from_slice(&value.to_be_bytes());
    }
    assert_eq!(encode(ticket()), b);
    assert_eq!(decode(&b), Ok(ticket()));
    for end in 0..b.len() {
        assert!(decode(&b[..end]).is_err());
    }
    b.push(0);
    assert!(decode(&b).is_err());
}
#[test]
fn identities_direction_delivery_limits_and_deadlines_are_checked() {
    let b = encode(ticket());
    for start in [24, 40, 56] {
        let mut bad = b.clone();
        bad[start..start + 16].fill(0);
        assert_eq!(decode(&bad), Err(WireError::InvalidBinding));
    }
    assert!(
        input_ticket::decode(&b, &LIMITS, 8, Direction::HostToViewer, Delivery::Reliable).is_err()
    );
    assert_eq!(
        input_ticket::decode(&b, &LIMITS, 7, Direction::ViewerToHost, Delivery::Reliable),
        Err(WireError::WrongRole)
    );
    assert_eq!(
        input_ticket::decode(&b, &LIMITS, 7, Direction::HostToViewer, Delivery::Datagram),
        Err(WireError::WrongChannel)
    );
    for expiry in [0, 6, 5, 1_500_007, u64::MAX] {
        let mut bad = b.clone();
        bad[120..128].copy_from_slice(&expiry.to_be_bytes());
        assert_eq!(decode(&bad), Err(WireError::InvalidValue));
    }
    let low = ProtocolLimits::with_overrides(LimitOverrides {
        max_control_message_bytes: Some(127),
        ..Default::default()
    })
    .unwrap();
    assert!(
        input_ticket::decode(&b, &low, 7, Direction::HostToViewer, Delivery::Reliable).is_err()
    );
    let mut bad = b.clone();
    bad[7] = 0x48;
    assert!(decode(&bad).is_err());
}
#[test]
fn zero_generation_and_sequence_are_valid_but_no_secret_appears_in_debug() {
    let mut t = ticket();
    t.sequence = 0;
    t.issued_at_us = 0;
    t.expires_at_us = 1;
    t.credentials.view = InputView {
        geometry: DisplayGeometryGeneration::INITIAL,
        viewport: ViewportMappingGeneration::INITIAL,
        configuration: CodecConfigurationGeneration::INITIAL,
        recovery: RecoveryGeneration::INITIAL,
    };
    assert_eq!(decode(&encode(t)), Ok(t));
    let debug = format!("{t:?}");
    t.credentials.ticket = InputTicketId::from_raw(7);
    assert_eq!(debug, format!("{t:?}"));
    assert!(
        input_ticket::encode(
            t,
            &mut [0; 127],
            &LIMITS,
            7,
            Direction::HostToViewer,
            Delivery::Reliable
        )
        .is_err()
    );
}
