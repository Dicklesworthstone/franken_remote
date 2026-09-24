use fr_core::{ids::RemoteSessionId, limits::ProtocolLimits};
use fr_wire::{
    WireError,
    authority::Binding,
    closure::{self, CloseRequest, Reason},
    input::{InputDelivery as T, InputDirection as D},
    stream::RecordStream,
};

// Written from PROTOCOL_CLOSURE.md, not output captured from encode_request.
const FIXTURE: &[u8] = b"FRD0\x00\x00\x00\x1d\x00\x00\x00\x00\x00\x00\x00\x13\x00\x00\x00\x07\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x0d\x01\x00\x04";
fn binding() -> Binding {
    Binding {
        channel: 7,
        session: RemoteSessionId::from_raw(13),
    }
}
fn decode(bytes: &[u8]) -> Result<CloseRequest, WireError> {
    closure::decode_request(
        bytes,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        D::ViewerToHost,
        T::Reliable,
    )
}
#[test]
fn independent_close_fixture_and_every_stream_split() {
    let request = CloseRequest {
        reason: Reason::InspectionComplete,
    };
    assert_eq!(FIXTURE.len(), closure::REQUEST_BYTES);
    assert_eq!(decode(FIXTURE), Ok(request));
    let mut bytes = [0; closure::REQUEST_BYTES];
    let length = closure::encode_request(
        request,
        binding(),
        &ProtocolLimits::ABSOLUTE,
        &mut bytes,
        D::ViewerToHost,
        T::Reliable,
    )
    .unwrap();
    assert_eq!(length, bytes.len());
    assert_eq!(bytes, FIXTURE);
    for split in 0..=FIXTURE.len() {
        let mut stream = RecordStream::new(128, 7, 100).unwrap();
        assert_eq!(stream.push(&FIXTURE[..split], 0).unwrap(), split);
        assert_eq!(
            stream.push(&FIXTURE[split..], 1).unwrap(),
            FIXTURE.len() - split
        );
        assert_eq!(decode(stream.frame(1).unwrap().unwrap()), Ok(request));
        stream.consume(1).unwrap();
        stream.finish(1).unwrap();
    }
    for end in 0..FIXTURE.len() {
        assert!(decode(&FIXTURE[..end]).is_err());
    }
}
#[test]
fn session_identity_direction_and_reliable_binding_are_mandatory() {
    for invalid in [
        Binding {
            channel: 0,
            ..binding()
        },
        Binding {
            channel: 8,
            ..binding()
        },
        Binding {
            session: RemoteSessionId::from_raw(0),
            ..binding()
        },
        Binding {
            session: RemoteSessionId::from_raw(14),
            ..binding()
        },
    ] {
        assert_eq!(
            closure::decode_request(
                FIXTURE,
                invalid,
                &ProtocolLimits::ABSOLUTE,
                D::ViewerToHost,
                T::Reliable,
            ),
            Err(WireError::InvalidBinding)
        );
    }
    for (direction, delivery, error) in [
        (D::HostToViewer, T::Reliable, WireError::WrongRole),
        (D::ViewerToHost, T::Datagram, WireError::WrongChannel),
    ] {
        assert_eq!(
            closure::decode_request(
                FIXTURE,
                binding(),
                &ProtocolLimits::ABSOLUTE,
                direction,
                delivery,
            ),
            Err(error)
        );
        assert_eq!(
            closure::encode_request(
                CloseRequest {
                    reason: Reason::Requested,
                },
                binding(),
                &ProtocolLimits::ABSOLUTE,
                &mut [0; closure::REQUEST_BYTES],
                direction,
                delivery,
            ),
            Err(error)
        );
    }
    assert_eq!(
        closure::encode_request(
            CloseRequest {
                reason: Reason::Requested,
            },
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut [0; closure::REQUEST_BYTES - 1],
            D::ViewerToHost,
            T::Reliable,
        ),
        Err(WireError::BufferTooSmall)
    );
    let mut stream = RecordStream::negotiation(128, 100).unwrap();
    assert!(stream.push(FIXTURE, 0).is_err());
    assert_eq!(stream.allocated_bytes(), 0);
}
#[test]
fn unsupported_control_release_and_malformed_payloads_are_not_session_close() {
    for (offset, value, error) in [
        (40, 0, WireError::UnsupportedKind),
        (40, 2, WireError::InvalidValue),
        (42, 0, WireError::InvalidValue),
        (42, 5, WireError::InvalidValue),
        (39, 14, WireError::InvalidBinding),
    ] {
        let mut bytes = FIXTURE.to_vec();
        bytes[offset] = value;
        assert_eq!(decode(&bytes), Err(error));
    }
    let mut bytes = FIXTURE.to_vec();
    bytes.push(0);
    assert_eq!(decode(&bytes), Err(WireError::TrailingBytes));
    bytes[15] += 1;
    assert_eq!(decode(&bytes), Err(WireError::TrailingBytes));
    for reason in [
        Reason::Requested,
        Reason::ClientStopping,
        Reason::ClientFailure,
        Reason::InspectionComplete,
    ] {
        let request = CloseRequest { reason };
        let mut bytes = [0; closure::REQUEST_BYTES];
        closure::encode_request(
            request,
            binding(),
            &ProtocolLimits::ABSOLUTE,
            &mut bytes,
            D::ViewerToHost,
            T::Reliable,
        )
        .unwrap();
        assert_eq!(decode(&bytes), Ok(request));
    }
}
