#![cfg(target_os = "linux")]
use asupersync::{
    bytes::BytesMut,
    codec::{Decoder, Encoder},
    net::atp::protocol::{AtpFrameCodec, Frame, FrameType, ProtocolVersion},
};
use fr_files::{
    atp::{self, ObjectRecord},
    receive::MAX_CHUNK_BYTES,
    session::Error,
};
fn upstream(frame: Frame) -> Vec<u8> {
    let mut bytes = BytesMut::new();
    AtpFrameCodec::new().encode(frame, &mut bytes).unwrap();
    bytes.to_vec()
}
#[test]
fn exact_upstream_single_entry_layout_and_frame_bytes() {
    // ATP varints: v0, ObjectData=0x0102, payload length 15, zero extensions;
    // upstream reliable transport payload: u32 entry0, u64 offset7, data "abc".
    let golden = [
        0, 0x41, 2, 15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 7, b'a', b'b', b'c',
    ];
    let ours = atp::encode_data(7, b"abc").unwrap();
    assert_eq!(ours, golden);
    let mut bytes = BytesMut::from(ours.as_slice());
    let frame = AtpFrameCodec::new().decode(&mut bytes).unwrap().unwrap();
    assert_eq!(frame.frame_type(), FrameType::ObjectData);
    assert_eq!(&frame.payload[4..12], &7u64.to_be_bytes());
    assert_eq!(&frame.payload[12..], b"abc");
    let accepted = ObjectRecord::decode(&golden).unwrap();
    assert_eq!(accepted.data(), Some((7, &b"abc"[..])));
    assert!(
        ObjectRecord::decode(&atp::encode_complete().unwrap())
            .unwrap()
            .data()
            .is_none()
    );
}
#[test]
fn unsupported_metadata_versions_extensions_and_trailing_frames_refuse() {
    let mut payload = vec![0; 12];
    payload.push(4);
    for frame in [
        Frame::empty(FrameType::KeepAlive).unwrap(),
        Frame::new(ProtocolVersion::V0, FrameType::ObjectComplete, vec![0]).unwrap(),
        Frame::new(ProtocolVersion::V0, FrameType::ObjectData, vec![0; 12]).unwrap(),
    ] {
        assert_eq!(
            ObjectRecord::decode(&upstream(frame)).unwrap_err(),
            Error::Protocol
        );
    }
    payload[3] = 1;
    assert!(
        ObjectRecord::decode(&upstream(
            Frame::new(ProtocolVersion::V0, FrameType::ObjectData, payload).unwrap()
        ))
        .is_err()
    );
    let mut frame = Frame::empty(FrameType::ObjectComplete).unwrap();
    frame.header.extensions.insert(1, vec![1]);
    assert!(ObjectRecord::decode(&upstream(frame)).is_err());
    let mut version = atp::encode_complete().unwrap();
    version[0] = 1;
    assert!(ObjectRecord::decode(&version).is_err());
    let mut double = atp::encode_complete().unwrap();
    double.extend_from_slice(&double.clone());
    assert!(ObjectRecord::decode(&double).is_err());
}
#[test]
fn bounded_truncation_and_forged_length_never_reach_a_partial_object() {
    let record = atp::encode_data(0, &vec![7; MAX_CHUNK_BYTES]).unwrap();
    assert_eq!(
        ObjectRecord::decode(&record)
            .unwrap()
            .data()
            .unwrap()
            .1
            .len(),
        MAX_CHUNK_BYTES
    );
    for n in 0..record.len().min(32) {
        assert!(ObjectRecord::decode(&record[..n]).is_err());
    }
    assert!(ObjectRecord::decode(&record[..record.len() - 1]).is_err());
    assert!(ObjectRecord::decode(&vec![0; atp::MAX_FRAME_BYTES + 1]).is_err());
    assert!(atp::encode_data(0, b"").is_err());
    assert!(atp::encode_data(0, &vec![0; MAX_CHUNK_BYTES + 1]).is_err());
}
