//! Real wire records in the production receive-custody owner. These tests do
//! not qualify native input submission or a live network path.
use super::*;
use fr_core::{
    ids::{
        CodecConfigurationGeneration, DisplayGeometryGeneration, InputLeaseId, InputTicketId,
        RecoveryGeneration, RemoteSessionId, ViewportMappingGeneration,
    },
    input::{DesktopPoint, InputCredentials, InputEvent, InputRequest, InputView},
    limits::ProtocolLimits,
};
use fr_wire::input::encode_input;

const BINDING: u32 = 3;
const POINT: DesktopPoint = DesktopPoint { x: 12, y: -7 };

fn route() -> DatagramRoute {
    DatagramRoute {
        binding: BINDING,
        kind: POINTER_PACKET,
        outbound: false,
    }
}

fn credentials() -> InputCredentials {
    InputCredentials {
        session: RemoteSessionId::from_raw(11),
        lease: InputLeaseId::from_raw(12),
        ticket: InputTicketId::from_raw(13),
        view: InputView {
            geometry: DisplayGeometryGeneration::INITIAL,
            viewport: ViewportMappingGeneration::INITIAL,
            configuration: CodecConfigurationGeneration::INITIAL,
            recovery: RecoveryGeneration::INITIAL,
        },
    }
}

fn packet(credentials: InputCredentials, sequence: u64, position: DesktopPoint) -> Bytes {
    let mut bytes = [0; fr_wire::HEADER_BYTES + fr_wire::input::INPUT_PREFIX_BYTES + 8];
    let size = encode_input(
        InputRequest {
            credentials,
            sequence,
            event: InputEvent::Pointer { position },
        },
        &mut bytes,
        &ProtocolLimits::ABSOLUTE,
        BINDING,
        InputDirection::ViewerToHost,
        InputDelivery::Datagram,
    )
    .unwrap();
    assert_eq!(size, bytes.len());
    Bytes::copy_from_slice(&bytes)
}

fn decode(bytes: &[u8]) -> InputRequest<'_> {
    decode_input(
        bytes,
        &ProtocolLimits::ABSOLUTE,
        BINDING,
        InputDirection::ViewerToHost,
        InputDelivery::Datagram,
    )
    .unwrap()
}

fn consume(
    inbox: &mut PendingDatagrams,
    at: u64,
    expected: DatagramRoute,
) -> (Vec<u8>, DatagramReceipt) {
    let mut received = None;
    assert_eq!(
        inbox.receive_one(at, &mut |route, bytes, receipt| {
            assert_eq!(route, Route::Datagram(expected));
            received = Some((bytes.to_vec(), receipt));
            Ok(Disposition::Consumed)
        }),
        Ok(1)
    );
    received.unwrap()
}

#[test]
fn blocked_pointer_motion_keeps_only_the_latest_canonical_state() {
    let mut inbox = PendingDatagrams::new();
    for sequence in 0..128 {
        let position = DesktopPoint {
            x: i32::try_from(sequence).unwrap(),
            y: -9,
        };
        assert!(
            inbox
                .hold(
                    route(),
                    packet(credentials(), sequence, position),
                    10 + sequence,
                    1000
                )
                .unwrap()
        );
        assert!(!inbox.waiting(10 + sequence));
        assert_eq!(inbox.bytes(), 120);
        assert_eq!(
            inbox.receive_one(10 + sequence, &mut |_, _, _| Ok(Disposition::Blocked)),
            Ok(0)
        );
    }
    let (bytes, receipt) = consume(&mut inbox, 138, route());
    let latest = decode(&bytes);
    assert_eq!(latest.credentials, credentials());
    assert_eq!(latest.sequence, 127);
    assert_eq!(
        latest.event,
        InputEvent::Pointer {
            position: DesktopPoint { x: 127, y: -9 }
        }
    );
    assert_eq!(receipt.received_at_micros(), 137);
    assert_eq!(receipt.expires_at_micros(), 1137);
    assert_eq!(inbox.bytes(), 0);
}

#[test]
fn duplicate_and_reordered_states_do_not_replace_or_renew_the_held_pointer() {
    let mut inbox = PendingDatagrams::new();
    let original = packet(credentials(), u64::MAX, POINT);
    assert!(inbox.hold(route(), original.clone(), 10, 100).unwrap());
    for sequence in [u64::MAX, 0, 1, u64::MAX - 1] {
        // A repeated pointer sequence is obsolete even with different position
        // bytes, exactly as the final pointer owner treats it; it is no action.
        let incoming = packet(credentials(), sequence, DesktopPoint { x: 99, y: 98 });
        assert!(inbox.hold(route(), incoming, 100, 10_000).unwrap());
        assert!(!inbox.waiting(100));
        assert_eq!(inbox.bytes(), original.len());
    }
    assert_eq!(
        inbox.receive_one(109, &mut |_, bytes, receipt| {
            assert_eq!(bytes, original.as_ref());
            assert_eq!(receipt.received_at_micros(), 10);
            assert_eq!(receipt.expires_at_micros(), 110);
            Ok(Disposition::Blocked)
        }),
        Ok(0)
    );
    assert_eq!(
        inbox.receive_one(110, &mut |_, _, _| panic!("expired original pointer")),
        Ok(0)
    );
    assert_eq!(inbox.bytes(), 0);
}

#[test]
fn replacement_keeps_its_own_shorter_original_custody() {
    let mut inbox = PendingDatagrams::new();
    assert!(
        inbox
            .hold(route(), packet(credentials(), 0, POINT), 10, 1000)
            .unwrap()
    );
    assert!(
        inbox
            .hold(route(), packet(credentials(), 1, POINT), 100, 20)
            .unwrap()
    );
    assert!(
        inbox
            .hold(route(), packet(credentials(), 1, POINT), 110, 1000)
            .unwrap()
    );
    assert_eq!(
        inbox.receive_one(119, &mut |_, bytes, receipt| {
            assert_eq!(decode(bytes).sequence, 1);
            assert_eq!(receipt.received_at_micros(), 100);
            assert_eq!(receipt.expires_at_micros(), 120);
            Ok(Disposition::Blocked)
        }),
        Ok(0)
    );
    assert_eq!(
        inbox.receive_one(120, &mut |_, _, _| panic!("expired replacement")),
        Ok(0)
    );
}

#[test]
fn every_credential_and_view_field_preserves_the_original_parked_record() {
    let original = credentials();
    let mut alternatives = [original; 7];
    alternatives[0].session = RemoteSessionId::from_raw(21);
    alternatives[1].lease = InputLeaseId::from_raw(22);
    alternatives[2].ticket = InputTicketId::from_raw(23);
    alternatives[3].view.geometry = DisplayGeometryGeneration::from_raw(1);
    alternatives[4].view.viewport = ViewportMappingGeneration::from_raw(1);
    alternatives[5].view.configuration = CodecConfigurationGeneration::from_raw(1);
    alternatives[6].view.recovery = RecoveryGeneration::from_raw(1);
    for changed in alternatives {
        let mut inbox = PendingDatagrams::new();
        assert!(
            inbox
                .hold(route(), packet(original, 1, POINT), 10, 100)
                .unwrap()
        );
        assert!(
            !inbox
                .hold(route(), packet(changed, 2, POINT), 20, 40)
                .unwrap()
        );
        assert!(inbox.waiting(20));
        assert_eq!(inbox.bytes(), 240);
        let (first, _) = consume(&mut inbox, 25, route());
        assert_eq!(decode(&first).credentials, original);
        assert!(!inbox.waiting(25));
        let (second, receipt) = consume(&mut inbox, 30, route());
        assert_eq!(decode(&second).credentials, changed);
        assert_eq!(decode(&second).sequence, 2);
        assert_eq!(receipt.received_at_micros(), 20);
        assert_eq!(receipt.expires_at_micros(), 60);
    }
}

#[test]
fn a_promoted_pointer_can_coalesce_only_after_its_old_scope_was_delivered() {
    let mut inbox = PendingDatagrams::new();
    let mut renewed = credentials();
    renewed.ticket = InputTicketId::from_raw(99);
    assert!(
        inbox
            .hold(route(), packet(credentials(), 1, POINT), 10, 100)
            .unwrap()
    );
    assert!(
        !inbox
            .hold(route(), packet(renewed, 2, POINT), 20, 40)
            .unwrap()
    );
    assert_eq!(decode(&consume(&mut inbox, 25, route()).0).sequence, 1);
    assert!(!inbox.waiting(25));
    assert_eq!(
        inbox.receive_one(29, &mut |_, bytes, receipt| {
            assert_eq!(decode(bytes).sequence, 2);
            assert_eq!(receipt.received_at_micros(), 20);
            assert_eq!(receipt.expires_at_micros(), 60);
            Ok(Disposition::Blocked)
        }),
        Ok(0)
    );
    assert!(
        inbox
            .hold(route(), packet(renewed, 3, POINT), 30, 100)
            .unwrap()
    );
    assert!(!inbox.waiting(30));
    let (bytes, receipt) = consume(&mut inbox, 31, route());
    assert_eq!(decode(&bytes).sequence, 3);
    assert_eq!(receipt.received_at_micros(), 30);
    assert_eq!(receipt.expires_at_micros(), 130);
}

fn extended(flags: u8) -> Bytes {
    let mut bytes = packet(credentials(), 2, POINT).to_vec();
    bytes.extend_from_slice(&[0, 1, 0, flags, 0, 0, 0, 0]);
    bytes[12..16].copy_from_slice(&104_u32.to_be_bytes());
    bytes[20..24].copy_from_slice(&8_u32.to_be_bytes());
    Bytes::from(bytes)
}

#[test]
fn extensions_and_malformed_payloads_never_disappear_by_supersession() {
    let good = packet(credentials(), 1, POINT);
    let mut truncated = packet(credentials(), 0, POINT).to_vec();
    truncated.pop();
    truncated[12..16].copy_from_slice(&95_u32.to_be_bytes());
    let optional = extended(0);
    assert_eq!(decode(&optional).sequence, 2);
    for special in [Bytes::from(truncated), extended(1), optional] {
        for (first, second) in [
            (good.clone(), special.clone()),
            (special.clone(), good.clone()),
        ] {
            let mut inbox = PendingDatagrams::new();
            assert!(inbox.hold(route(), first.clone(), 10, 100).unwrap());
            assert!(!inbox.hold(route(), second.clone(), 20, 100).unwrap());
            assert!(inbox.waiting(20));
            assert_eq!(inbox.bytes(), first.len() + second.len());
            assert_eq!(consume(&mut inbox, 21, route()).0, first.as_ref());
            assert!(!inbox.waiting(21));
            assert_eq!(consume(&mut inbox, 22, route()).0, second.as_ref());
        }
    }
}

#[test]
fn pointer_flood_does_not_park_independent_audio_or_video() {
    let mut inbox = PendingDatagrams::new();
    for sequence in 0..128 {
        assert!(
            inbox
                .hold(route(), packet(credentials(), sequence, POINT), 10, 1000)
                .unwrap()
        );
    }
    let audio = DatagramRoute {
        binding: 1,
        kind: AUDIO_PACKET,
        outbound: false,
    };
    let video = DatagramRoute {
        binding: 2,
        kind: 0x0034,
        outbound: false,
    };
    for lane in [audio, video] {
        assert!(
            inbox
                .hold(lane, Bytes::from(vec![7; 24]), 10, 1000)
                .unwrap()
        );
    }
    let mut delivered = Vec::new();
    for _ in 0..2 {
        assert_eq!(
            inbox.receive_one(11, &mut |lane, _, _| {
                if lane == Route::Datagram(route()) {
                    Ok(Disposition::Blocked)
                } else {
                    delivered.push(lane);
                    Ok(Disposition::Consumed)
                }
            }),
            Ok(1)
        );
    }
    assert_eq!(delivered, [Route::Datagram(audio), Route::Datagram(video)]);
    assert_eq!(inbox.bytes(), 120);
    assert_eq!(decode(&consume(&mut inbox, 12, route()).0).sequence, 127);
}

#[test]
fn retirement_close_and_handler_failure_preserve_the_existing_fences() {
    let audio = DatagramRoute {
        binding: 1,
        kind: AUDIO_PACKET,
        outbound: false,
    };
    let mut inbox = PendingDatagrams::new();
    assert!(
        inbox
            .hold(route(), packet(credentials(), 1, POINT), 10, 1000)
            .unwrap()
    );
    assert!(
        inbox
            .hold(route(), packet(credentials(), 2, POINT), 20, 1000)
            .unwrap()
    );
    assert_eq!(
        inbox.receive_one(21, &mut |_, _, _| Err(())),
        Err(Error::Handler)
    );
    assert_eq!(inbox.bytes(), 120);
    assert!(
        inbox
            .hold(audio, Bytes::from(vec![7; 24]), 21, 1000)
            .unwrap()
    );
    inbox.retire(BINDING);
    assert_eq!(inbox.bytes(), 24);
    assert_eq!(consume(&mut inbox, 22, audio).0, [7; 24]);
    assert!(
        inbox
            .hold(route(), packet(credentials(), 3, POINT), 30, 1000)
            .unwrap()
    );
    inbox.clear();
    assert_eq!(inbox.bytes(), 0);
    assert_eq!(
        inbox.receive_one(31, &mut |_, _, _| panic!("closed custody")),
        Ok(0)
    );
}
