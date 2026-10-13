//! The pinned Asupersync must decode the RFC 9000 frames that 0.5.0 refused:
//! `NEW_TOKEN` (0x07), `NEW_CONNECTION_ID` (0x18) and `RETIRE_CONNECTION_ID`
//! (0x19). An interoperable peer sends them after the handshake, and a
//! connection that cannot decode them fails with a frame-encoding error.
//! Codec only: not interop with any other QUIC stack.
use asupersync::net::{atp::protocol::QuicFrame, quic_native::NativeQuicConnection};

#[test]
fn the_pin_decodes_new_token_new_connection_id_and_retire_connection_id() {
    // Wire bytes written by hand from RFC 9000 sections 19.7, 19.15 and
    // 19.16, not produced by the encoder under test.
    let mut payload = vec![0x07, 0x04, 0xde, 0xad, 0xbe, 0xef];
    // Sequence 2, retire prior to 1, an 8-byte connection ID, a reset token.
    payload.extend([0x18, 0x02, 0x01, 0x08]);
    payload.extend([0x11; 8]);
    payload.extend([0x22; 16]);
    payload.extend([0x19, 0x01]);
    let frames = NativeQuicConnection::decode_frames(&payload).unwrap();
    assert_eq!(frames.len(), 3, "{frames:?}");
    let QuicFrame::NewToken { token } = &frames[0] else {
        panic!("{:?}", frames[0]);
    };
    assert_eq!(&token[..], [0xde, 0xad, 0xbe, 0xef]);
    let QuicFrame::NewConnectionId {
        sequence_number,
        retire_prior_to,
        connection_id,
        stateless_reset_token,
    } = &frames[1]
    else {
        panic!("{:?}", frames[1]);
    };
    assert_eq!((sequence_number.0, retire_prior_to.0), (2, 1));
    assert_eq!(connection_id.as_bytes(), [0x11; 8]);
    assert_eq!(*stateless_reset_token, [0x22; 16]);
    let QuicFrame::RetireConnectionId { sequence_number } = &frames[2] else {
        panic!("{:?}", frames[2]);
    };
    assert_eq!(sequence_number.0, 1);
}

#[test]
fn a_truncated_new_connection_id_is_refused_not_misread() {
    // The reset token is cut short: the frame must not decode.
    let mut payload = vec![0x18, 0x02, 0x01, 0x08];
    payload.extend([0x11; 8]);
    payload.extend([0x22; 15]);
    assert!(NativeQuicConnection::decode_frames(&payload).is_err());
}
