//! Hostile local IPC tests, not native codec or remote-session evidence.
use super::*;
use std::io::Cursor;
fn config() -> AudioStreamConfig {
    AudioStreamConfig::new(
        AudioDirection::Downlink,
        AudioGeneration::INITIAL,
        AudioChannels::Mono,
        10,
        20,
    )
    .unwrap()
}
#[test]
fn header_has_a_fixed_versioned_layout_and_all_partial_headers_refuse() {
    let mut h = Header::new(config(), 1, CONFIG).unwrap();
    h.bytes = 8;
    let bytes = h.encode(false);
    assert_eq!(&bytes[..8], b"FROP\x01\x01\0\0");
    assert_eq!(&bytes[8..16], &1u64.to_be_bytes());
    assert_eq!(&bytes[16..40], &[0; 24]); // zero is the legitimate INITIAL generation
    assert_eq!(&bytes[40..], &[0, 0, 0, 8, 1, 224, 1, 0]);
    assert!(Header::read(&mut Cursor::new(bytes), false).unwrap() == h);
    for n in 0..HEADER {
        assert!(Header::read(&mut Cursor::new(&bytes[..n]), false).is_err());
    }
    for (index, value) in [
        (0, b'Q'),
        (4, 2),
        (5, 0),
        (5, 4),
        (6, 1),
        (7, 1),
        (46, 0),
        (46, 3),
        (47, 2),
    ] {
        let mut bad = bytes;
        bad[index] = value;
        assert!(Header::read(&mut Cursor::new(bad), false).is_err());
    }
}
#[test]
fn configuration_checks_original_kind_transaction_shape_and_limits() {
    let mut header = Header::new(config(), 1, CONFIG).unwrap();
    header.bytes = 8;
    let body = config_body(config(), CodecLimits::ABSOLUTE).unwrap();
    let (actual, limits) = read_config(header, &mut Cursor::new(body)).unwrap();
    assert_eq!(actual, config());
    assert_eq!(limits, CodecLimits::ABSOLUTE);
    for mut bad in [header; 6].into_iter().enumerate() {
        match bad.0 {
            0 => bad.1.kind = PACKET,
            1 => bad.1.serial = 2,
            2 => bad.1.bytes = 9,
            3 => bad.1.at = 1,
            4 => bad.1.sequence = 1,
            _ => bad.1.samples = 960,
        }
        assert!(read_config(bad.1, &mut Cursor::new(body)).is_err());
    }
    for n in 0..8 {
        assert!(read_config(header, &mut Cursor::new(&body[..n])).is_err());
    }
    for range in [4..6, 6..8] {
        let mut bad = body;
        bad[range].fill(0xff);
        assert!(read_config(header, &mut Cursor::new(bad)).is_err());
    }
}
#[test]
fn reply_identity_counts_and_epoch_are_checked_before_pcm_allocation_or_read() {
    let mut request = Header::new(config(), 2, PACKET).unwrap();
    request.at = 100;
    request.bytes = 20;
    let expected = request.pcm_reply();
    let mut data = expected.encode(true).to_vec();
    data.extend_from_slice(&vec![0x12; 960]);
    let pcm = read_pcm(&mut Cursor::new(&data), request).unwrap();
    assert_eq!(pcm.samples_per_channel(), 480);
    assert_eq!(pcm.timestamp_samples(), 100);
    assert!(pcm.samples().iter().all(|v| *v == 0x1212));
    for index in [0, 4, 5, 6, 8, 16, 24, 32, 40, 44, 46, 47] {
        let mut invalid = data[..HEADER].to_vec();
        invalid[index] ^= 1;
        let mut cursor = Cursor::new(invalid);
        assert!(read_pcm(&mut cursor, request).is_err());
        assert_eq!(
            cursor.position(),
            48,
            "refuse mismatched header before payload"
        );
    }
    for n in [0, 1, HEADER, HEADER + 1, data.len() - 1] {
        assert!(read_pcm(&mut Cursor::new(&data[..n]), request).is_err());
    }
}
#[test]
fn oversized_packets_do_not_read_payload() {
    let mut h = Header::new(config(), 2, PACKET).unwrap();
    h.bytes = 1276;
    let mut input = Cursor::new([0u8; 8]);
    assert!(matches!(
        read_packet(h, CodecLimits::ABSOLUTE, &mut input),
        Err(Error::BufferOverflow)
    ));
    assert_eq!(input.position(), 0);
}
