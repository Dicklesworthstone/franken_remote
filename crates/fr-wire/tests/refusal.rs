use fr_wire::{
    WireError,
    input::InputDelivery::{Datagram, Reliable},
    input_result::Stage,
    negotiation,
    refusal::{self, Reason, Refused},
    stream::{RecordStream, StreamError},
};

// Hand-written from PROTOCOL_REFUSAL.md, not encoder-generated snapshots.
const BOOTSTRAP: &[u8] = b"FRD0\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x00\x00\x0a\x00\x00";
const BOUND: &[u8] = b"FRD0\x00\x00\x00\x04\x00\x00\x00\x00\x00\x00\x00\x0d\x00\x00\x00\x07\x00\x00\x00\x00\x00\x0d\x01\x00\x00\x00\x00\x00\x00\x00\x09\x01\x01";

#[test]
fn independent_fixtures_decode_and_encode_exactly() {
    assert_eq!(BOOTSTRAP.len(), refusal::MIN_BYTES);
    assert_eq!(BOUND.len(), refusal::MAX_BYTES);
    for (bytes, binding, message) in [
        (
            BOOTSTRAP,
            0,
            Refused::connection(Reason::ControlUnavailable),
        ),
        (
            BOUND,
            7,
            Refused {
                reason: Reason::Expired,
                operation: Some(9),
                stage: Some(Stage::SubmittedToOs),
            },
        ),
    ] {
        assert_eq!(refusal::decode(bytes, binding, 4096, Reliable), Ok(message));
        let mut out = [0; refusal::MAX_BYTES];
        let len = refusal::encode(message, binding, 4096, &mut out, Reliable).unwrap();
        assert_eq!(&out[..len], bytes);
        assert_eq!(
            negotiation::decode(bytes, 4096, binding),
            Err(negotiation::Error::Refused(message))
        );
        for end in 0..bytes.len() {
            assert!(refusal::decode(&bytes[..end], binding, 4096, Reliable).is_err());
        }
    }
}

#[test]
fn refusal_crosses_every_incremental_framing_boundary() {
    for split in 0..=BOOTSTRAP.len() {
        let mut stream = RecordStream::negotiation(4096, 100).unwrap();
        assert_eq!(stream.push(&BOOTSTRAP[..split], 0).unwrap(), split);
        assert_eq!(
            stream.push(&BOOTSTRAP[split..], 1).unwrap(),
            BOOTSTRAP.len() - split
        );
        assert_eq!(
            negotiation::decode(stream.frame(1).unwrap().unwrap(), 4096, 0),
            Err(negotiation::Error::Refused(Refused::connection(
                Reason::ControlUnavailable
            )))
        );
        stream.consume(1).unwrap();
        stream.finish(1).unwrap();
    }
}

#[test]
fn wrong_binding_channel_limits_and_short_buffers_are_rejected() {
    for (bytes, binding) in [(BOOTSTRAP, 0), (BOUND, 7)] {
        assert_eq!(
            refusal::decode(bytes, binding + 1, 4096, Reliable),
            Err(WireError::InvalidBinding)
        );
        assert_eq!(
            refusal::decode(bytes, binding, 4096, Datagram),
            Err(WireError::WrongChannel)
        );
        assert_eq!(
            refusal::decode(bytes, binding, bytes.len() - 1, Reliable),
            Err(WireError::ResourceLimit)
        );
        let message = refusal::decode(bytes, binding, 4096, Reliable).unwrap();
        let mut out = [0; refusal::MAX_BYTES];
        assert_eq!(
            refusal::encode(message, binding, 4096, &mut out, Datagram),
            Err(WireError::WrongChannel)
        );
        assert_eq!(
            refusal::encode(message, binding, bytes.len() - 1, &mut out, Reliable),
            Err(WireError::ResourceLimit)
        );
        assert_eq!(
            refusal::encode(
                message,
                binding,
                4096,
                &mut out[..bytes.len() - 1],
                Reliable
            ),
            Err(WireError::BufferTooSmall)
        );
    }
}

#[test]
fn malformed_values_and_framing_never_become_a_peer_refusal() {
    for (index, value) in [
        (0, b'X'),
        (5, 1),
        (7, 5),
        (8, 1),
        (15, 255),
        (23, 5),
        (25, 0),
        (25, 255),
        (26, 2),
        (27, 2),
    ] {
        let mut bytes = BOOTSTRAP.to_vec();
        bytes[index] = value;
        assert!(refusal::decode(&bytes, 0, 4096, Reliable).is_err());
        assert!(!matches!(
            negotiation::decode(&bytes, 4096, 0),
            Err(negotiation::Error::Refused(_))
        ));
    }
    let mut bytes = BOUND.to_vec();
    bytes[36] = 3;
    assert_eq!(
        refusal::decode(&bytes, 7, 4096, Reliable),
        Err(WireError::InvalidValue)
    );
    bytes = BOOTSTRAP.to_vec();
    bytes.push(0);
    assert_eq!(
        refusal::decode(&bytes, 0, 4096, Reliable),
        Err(WireError::TrailingBytes)
    );
    bytes[15] = 5;
    assert_eq!(
        refusal::decode(&bytes, 0, 4096, Reliable),
        Err(WireError::TrailingBytes)
    );
}

#[test]
fn effect_stage_needs_an_operation_and_an_established_binding() {
    let mut out = [0; refusal::MAX_BYTES];
    for operation in [None, Some(0), Some(u64::MAX)] {
        let message = Refused {
            reason: Reason::Expired,
            operation,
            stage: Some(Stage::Observed),
        };
        assert_eq!(
            refusal::encode(message, 0, 4096, &mut out, Reliable),
            Err(WireError::InvalidValue)
        );
        if operation.is_none() {
            assert_eq!(
                refusal::encode(message, 7, 4096, &mut out, Reliable),
                Err(WireError::InvalidValue)
            );
        }
    }
    let mut bytes = BOUND.to_vec();
    bytes[16..20].fill(0);
    assert_eq!(
        refusal::decode(&bytes, 0, 4096, Reliable),
        Err(WireError::InvalidValue)
    );
}

#[test]
fn refusal_trickle_does_not_extend_framing_deadline() {
    let mut stream = RecordStream::negotiation(4096, 10).unwrap();
    stream.push(&BOOTSTRAP[..1], 0).unwrap();
    stream.push(&BOOTSTRAP[1..24], 9).unwrap();
    assert_eq!(stream.push(&BOOTSTRAP[24..], 10), Err(StreamError::Expired));
    assert_eq!(stream.allocated_bytes(), 0);
}
